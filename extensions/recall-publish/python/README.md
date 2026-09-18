# recall-publish redaction environment

`recall publish doctor --install` copies these files into the Recall data
directory and runs `uv sync --frozen`. `scan.py` then serves line-delimited JSON
redaction requests on stdin.
