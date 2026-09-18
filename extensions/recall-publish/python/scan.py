import json
import sys

from presidio_analyzer import AnalyzerEngine, Pattern, PatternRecognizer, RecognizerResult
from presidio_analyzer.nlp_engine import NlpEngineProvider
from presidio_anonymizer import AnonymizerEngine
from presidio_anonymizer.entities import OperatorConfig

MODELS = {"en": "en_core_web_lg", "zh": "zh_core_web_lg"}

DENIED_ENTITIES = frozenset(
    {
        "DATE_TIME",
        "NRP",
        "URL",
        "ORGANIZATION",
        "US_DRIVER_LICENSE",
        "US_BANK_NUMBER",
        "MEDICAL_LICENSE",
    }
)

SCORE_THRESHOLD = 0.5

EXTRA_PATTERNS = {
    "CN_ID_CARD": Pattern(
        "cn-id-card",
        r"\b[1-9]\d{5}(?:19|20)\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\d{3}[\dXx]\b",
        0.9,
    ),
    "CN_PHONE_NUMBER": Pattern(
        "cn-phone-number", r"(?<![0-9A-Za-z])1[3-9]\d{9}(?![0-9A-Za-z])", 0.85
    ),
}


def build_analyzer(languages):
    configuration = {
        "nlp_engine_name": "spacy",
        "models": [
            {"lang_code": language, "model_name": MODELS[language]} for language in languages
        ],
    }
    engine = NlpEngineProvider(nlp_configuration=configuration).create_engine()
    analyzer = AnalyzerEngine(nlp_engine=engine, supported_languages=list(languages))
    for entity, pattern in EXTRA_PATTERNS.items():
        for language in languages:
            analyzer.registry.add_recognizer(
                PatternRecognizer(
                    supported_entity=entity,
                    supported_language=language,
                    patterns=[pattern],
                )
            )
    return analyzer


def merge(spans):
    ordered = sorted(
        spans, key=lambda span: (span.start, -span.score, -(span.end - span.start))
    )
    kept = []
    for span in ordered:
        if kept and span.start < kept[-1].end:
            if span.end > kept[-1].end:
                kept[-1].end = span.end
            continue
        kept.append(span)
    return kept


def analyze_leaf(analyzer, text, languages, allow_identities, allowed_entities):
    results = []
    for language in languages:
        for result in analyzer.analyze(
            text=text, language=language, allow_list=allow_identities
        ):
            if result.entity_type in DENIED_ENTITIES:
                continue
            if result.entity_type in allowed_entities:
                continue
            if result.score < SCORE_THRESHOLD:
                continue
            results.append(result)
    return results


def handle(analyzer, anonymizer, request):
    languages = [language for language in request["languages"] if language in MODELS]
    allow_identities = request.get("allow_identities") or None
    allowed_entities = frozenset(request.get("allow_entities") or ())

    injected = {}
    for span in request.get("spans", ()):
        injected.setdefault(span["i"], []).append(
            RecognizerResult(
                entity_type=span["entity"], start=span["start"], end=span["end"], score=1.0
            )
        )

    leaves = []
    for leaf in request["leaves"]:
        index = leaf["i"]
        text = leaf["text"]
        results = analyze_leaf(
            analyzer, text, languages, allow_identities, allowed_entities
        )
        results.extend(injected.get(index, ()))
        results = merge(results)
        if not results:
            continue
        operators = {
            result.entity_type: OperatorConfig(
                "replace", {"new_value": f"[REDACTED:{result.entity_type}]"}
            )
            for result in results
        }
        redacted = anonymizer.anonymize(
            text=text,
            analyzer_results=results,
            operators=operators,
            merge_entities_with_spaces=False,
        )
        leaves.append(
            {
                "i": index,
                "text": redacted.text,
                "spans": [
                    {
                        "entity": result.entity_type,
                        "start": result.start,
                        "end": result.end,
                    }
                    for result in results
                ],
            }
        )
    return {"leaves": leaves}


def main():
    analyzer = None
    anonymizer = AnonymizerEngine()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            if analyzer is None:
                analyzer = build_analyzer(
                    [language for language in request["languages"] if language in MODELS]
                )
            response = handle(analyzer, anonymizer, request)
        except Exception as error:
            response = {"error": f"{type(error).__name__}: {error}"}
        sys.stdout.write(json.dumps(response, ensure_ascii=False))
        sys.stdout.write("\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
