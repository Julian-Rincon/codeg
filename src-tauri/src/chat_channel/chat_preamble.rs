//! The general-chat system preamble injected into the FIRST prompt of a new
//! Telegram-originated session, and the machinery that keeps it out of
//! anything derived from that prompt for display (titles, transcript
//! bubbles).
//!
//! This mirrors, at a much smaller scale, how `acp::agent_mentions` hides its
//! internal routing frame from history: the payload is wrapped in a pair of
//! markers that are vanishingly unlikely to occur in real prose, a full
//! [`strip_chat_preamble`] removes a complete wrapped span from turn text,
//! and [`cut_at_chat_preamble_marker`] blanks a title that starts inside the
//! preamble before the parser's own length cap ever reaches the real user
//! text that follows it (see `parsers::title_from_user_text`, whose cap is
//! far shorter than the preamble itself, so a title landing on the marker can
//! only be preamble, never a truncated mix of both).
//!
//! Unlike the route frame in `agent_mentions`, this marker carries no
//! machine-parsed payload and needs no byte-exact round-trip proof — it is
//! plain instruction text meant for the model to read, just not meant to
//! surface as a conversation title.

use std::sync::LazyLock;

/// Invisible (U+2063 INVISIBLE SEPARATOR) wrapper so the marker can't
/// plausibly appear in text a person typed, while staying short enough to
/// always survive inside a title's truncation cap (~100 chars), unlike the
/// preamble body itself.
const MARKER_START: &str = "\u{2063}CODEG-CHAT-PREAMBLE-START\u{2063}";
const MARKER_END: &str = "\u{2063}CODEG-CHAT-PREAMBLE-END\u{2063}";

/// Spanish system-style instruction prepended to the first prompt of a new
/// chat-channel session so the agent knows it is fielding Phantom's Telegram
/// general chat: identify what the user wants, delegate the parts another
/// agent/model handles better via `delegate_to_agent`, answer briefly in the
/// user's language, and use the `send_file` directive to attach a file.
pub const CHAT_PREAMBLE_TEXT: &str = "Estás atendiendo el chat general de Phantom por Telegram. \
Identifica qué pide el usuario. Si una parte la hace mejor otro agente/modelo según la guía de \
ruteo medida de la herramienta delegate_to_agent, delégala (pasa agent_type y model; cada modelo \
pertenece solo a su agente) e integra el resultado. Responde breve, en el idioma del usuario. Si \
pide un archivo, envíalo escribiendo en tu respuesta una línea \
`[[phantom:send_file <ruta absoluta> | <leyenda opcional>]]`.";

static WRAPPED_PREAMBLE: LazyLock<String> =
    LazyLock::new(|| format!("{MARKER_START}\n{CHAT_PREAMBLE_TEXT}\n{MARKER_END}"));

/// The preamble text, wrapped in its markers, ready to send as its own
/// `PromptInputBlock::Text` ahead of the user's real message.
pub fn wrap_chat_preamble() -> String {
    WRAPPED_PREAMBLE.clone()
}

/// `true` if `text` contains a (possibly truncated) chat preamble.
pub fn contains_chat_preamble(text: &str) -> bool {
    text.contains(MARKER_START)
}

/// Remove every complete `MARKER_START .. MARKER_END` span from `text`,
/// along with one adjacent newline so the removal doesn't leave a stray blank
/// line — mirrors `agent_mentions::strip_internal_agent_routes`'s trim.
///
/// An unterminated span (start marker present, no matching end — should
/// never happen for a frame Codeg itself appended intact, but a transport
/// that drops trailing content could produce one) is removed to the end of
/// the string rather than left in place, since a half-frame is never useful
/// to show either.
pub fn strip_chat_preamble(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(rel_start) = text[cursor..].find(MARKER_START) {
        let start = cursor + rel_start;
        out.push_str(&text[cursor..start]);

        match text[start..].find(MARKER_END) {
            Some(rel_end) => {
                cursor = start + rel_end + MARKER_END.len();
                if text.as_bytes().get(cursor) == Some(&b'\n') {
                    cursor += 1;
                }
            }
            None => {
                cursor = text.len();
            }
        }
    }
    out.push_str(&text[cursor..]);
    out.trim().to_string()
}

/// For the TITLE path only: a title is derived by capping a prompt's joined
/// text at a small character count (well under the preamble's length), so a
/// title landing on the marker can only ever be preamble noise — there is no
/// user text left to salvage inside that window. Blank the title outright
/// rather than trying to cut around it.
pub fn cut_at_chat_preamble_marker(text: &mut String) {
    if text.contains(MARKER_START) {
        text.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_preamble_round_trips_through_strip() {
        let wrapped = wrap_chat_preamble();
        assert!(contains_chat_preamble(&wrapped));
        assert_eq!(strip_chat_preamble(&wrapped), "");
    }

    #[test]
    fn strips_preamble_prefix_and_keeps_user_text_that_follows() {
        let text = format!("{}\n\nhaz esto por mí", wrap_chat_preamble());
        let stripped = strip_chat_preamble(&text);
        assert_eq!(stripped, "haz esto por mí");
    }

    #[test]
    fn text_without_preamble_is_unchanged() {
        let text = "just an ordinary agent reply";
        assert!(!contains_chat_preamble(text));
        assert_eq!(strip_chat_preamble(text), text);
    }

    #[test]
    fn strips_an_unterminated_span_to_end_of_string() {
        let text = format!("prefix {MARKER_START} half a preamble with no closing marker");
        let stripped = strip_chat_preamble(&text);
        assert_eq!(stripped, "prefix");
    }

    #[test]
    fn cut_at_marker_blanks_a_title_that_starts_inside_the_preamble() {
        // Simulates what a title deriver sees: the preamble is placed first,
        // so any capped title starting from byte 0 lands inside it.
        let mut title: String = wrap_chat_preamble().chars().take(100).collect();
        cut_at_chat_preamble_marker(&mut title);
        assert_eq!(title, "");
    }

    #[test]
    fn cut_at_marker_leaves_an_unrelated_title_untouched() {
        let mut title = "Fix the login flow".to_string();
        cut_at_chat_preamble_marker(&mut title);
        assert_eq!(title, "Fix the login flow");
    }

    #[test]
    fn preamble_mentions_the_send_file_directive_and_delegate_to_agent() {
        // Cheap guard against an accidental edit dropping the two concrete
        // mechanisms this preamble exists to advertise.
        assert!(CHAT_PREAMBLE_TEXT.contains("delegate_to_agent"));
        assert!(CHAT_PREAMBLE_TEXT.contains("[[phantom:send_file"));
    }
}
