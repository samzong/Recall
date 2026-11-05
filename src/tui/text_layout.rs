use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

pub(crate) fn wrap_visual_rows(text: &str, width: usize) -> Vec<String> {
    wrap_spans_to_lines(vec![Span::raw(text.to_string())], width)
        .into_iter()
        .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
        .collect()
}

pub(crate) fn wrap_spans_to_lines(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        let mut segment = String::new();
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch_width > 0 && current_width > 0 && current_width + ch_width > width {
                if !segment.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut segment), span.style));
                }
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
            segment.push(ch);
            current_width += ch_width;
            if current_width >= width && current_width > 0 {
                current.push(Span::styled(std::mem::take(&mut segment), span.style));
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
        }

        if !segment.is_empty() {
            current.push(Span::styled(segment, span.style));
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}
