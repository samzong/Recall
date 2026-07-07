use crate::types::{Message, Role, Session};

pub(crate) fn render_plain(session: &Session, messages: &[Message]) -> String {
    let mut content = String::new();
    content.push_str(&format!("Session: {}\n", session.title));
    content.push_str(&format!("ID: {}\n", session.id));
    content.push_str(&format!("Source: {}\n", session.source));
    content.push_str(&format!("Source ID: {}\n", session.source_id));
    if let Some(ref dir) = session.directory {
        content.push_str(&format!("Directory: {dir}\n"));
    }
    content.push_str(&format!(
        "Date: {}\n",
        chrono::DateTime::from_timestamp_millis(session.started_at)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    ));
    content.push_str(&format!("Messages: {}\n", messages.len()));
    content.push_str("\n---\n\n");

    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        content.push_str(&format!("## {role} [{}]\n\n{}\n\n", msg.seq, msg.content));
    }
    content
}
