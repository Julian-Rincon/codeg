//! Answer an agent's ask_user_question from the phone. Only the common case —
//! one single-select question with options — gets buttons; anything else stays
//! a notification to answer in Phantom. Tokens work like `permission_buttons`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::acp::question::{QuestionAnswer, QuestionAnswerItem, QuestionSpec};

use super::i18n::Lang;
use super::types::{ButtonStyle, InteractiveMessage, MessageButton, RichMessage};

const PREFIX: &str = "q:";
const SKIP: &str = "x";
// A blocked agent can wait days for an answer; entries are tiny.
const TTL: Duration = Duration::from_secs(7 * 24 * 3600);

struct Entry {
    connection_id: String,
    question_id: String,
    spec: QuestionSpec,
    created: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub struct Tapped {
    pub connection_id: String,
    pub question_id: String,
    pub answer: QuestionAnswer,
    pub label: String,
}

pub fn eligible(questions: &[QuestionSpec]) -> Option<&QuestionSpec> {
    match questions {
        [q] if !q.multi_select && !q.is_secret && !q.options.is_empty() => Some(q),
        _ => None,
    }
}

pub fn register(connection_id: &str, question_id: &str, spec: &QuestionSpec) -> String {
    let token = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, e| e.created.elapsed() < TTL);
    pending.insert(
        token.clone(),
        Entry {
            connection_id: connection_id.to_string(),
            question_id: question_id.to_string(),
            spec: spec.clone(),
            created: Instant::now(),
        },
    );
    token
}

/// The question was answered elsewhere: its buttons must not claim to answer it.
pub fn forget_question(question_id: &str) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, e| e.question_id != question_id);
}

/// Whether a tap would still answer something — without consuming the token.
pub fn is_live(data: &str) -> bool {
    let Some((token, _)) = data.strip_prefix(PREFIX).and_then(|r| r.rsplit_once(':')) else {
        return false;
    };
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(token)
        .is_some_and(|e| e.created.elapsed() < TTL)
}

pub fn is_question_callback(data: &str) -> bool {
    data.starts_with(PREFIX)
}

/// Resolve a tap. Single use, like a permission button.
pub fn take(data: &str) -> Option<Tapped> {
    let (token, choice) = data.strip_prefix(PREFIX)?.rsplit_once(':')?;
    let entry = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(token)?;
    if entry.created.elapsed() >= TTL {
        return None;
    }
    let (answer, label) = if choice == SKIP {
        (
            QuestionAnswer {
                answers: vec![],
                declined: true,
            },
            "Omitida".to_string(),
        )
    } else {
        let label = entry
            .spec
            .options
            .get(choice.parse::<usize>().ok()?)?
            .label
            .clone();
        (
            QuestionAnswer {
                answers: vec![QuestionAnswerItem {
                    question_id: entry.spec.id.clone(),
                    labels: vec![label.clone()],
                }],
                declined: false,
            },
            label,
        )
    };
    Some(Tapped {
        connection_id: entry.connection_id,
        question_id: entry.question_id,
        answer,
        label,
    })
}

/// One button per option plus "Skip" (the agent then decides on its own).
pub fn with_buttons(
    base: RichMessage,
    token: &str,
    spec: &QuestionSpec,
    lang: Lang,
) -> InteractiveMessage {
    let mut buttons: Vec<MessageButton> = spec
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| MessageButton {
            id: format!("{PREFIX}{token}:{i}"),
            label: o.label.clone(),
            style: ButtonStyle::Primary,
        })
        .collect();
    buttons.push(MessageButton {
        id: format!("{PREFIX}{token}:{SKIP}"),
        label: if lang == Lang::Es { "Omitir" } else { "Skip" }.to_string(),
        style: ButtonStyle::Default,
    });
    InteractiveMessage {
        base,
        buttons,
        callback_context: serde_json::json!({ "kind": "question" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::question::QuestionOption;

    fn spec(multi: bool, n: usize) -> QuestionSpec {
        QuestionSpec {
            id: "q1".into(),
            question: "¿Qué base de datos?".into(),
            header: "DB".into(),
            multi_select: multi,
            options: (0..n)
                .map(|i| QuestionOption {
                    label: format!("Opción {i}"),
                    description: String::new(),
                })
                .collect(),
            is_secret: false,
        }
    }

    #[test]
    fn only_a_single_single_select_question_gets_buttons() {
        assert!(eligible(&[spec(false, 2)]).is_some());
        assert!(eligible(&[spec(true, 2)]).is_none(), "multiselección");
        assert!(eligible(&[spec(false, 0)]).is_none(), "texto libre");
        assert!(
            eligible(&[spec(false, 2), spec(false, 2)]).is_none(),
            "varias preguntas"
        );
        let mut secret = spec(false, 2);
        secret.is_secret = true;
        assert!(eligible(&[secret]).is_none(), "secreto");
    }

    #[test]
    fn tap_answers_with_the_option_label_once() {
        let s = spec(false, 3);
        let token = register("conn", "qid", &s);
        let t = take(&format!("q:{token}:2")).expect("resuelve");
        assert_eq!(t.question_id, "qid");
        assert_eq!(t.answer.answers[0].question_id, "q1");
        assert_eq!(t.answer.answers[0].labels, vec!["Opción 2".to_string()]);
        assert!(!t.answer.declined);
        assert!(take(&format!("q:{token}:2")).is_none());
    }

    #[test]
    fn question_answered_elsewhere_kills_its_buttons() {
        let s = spec(false, 2);
        let token = register("conn", "qid-elsewhere", &s);
        forget_question("qid-elsewhere");
        assert!(take(&format!("q:{token}:0")).is_none());
    }

    #[test]
    fn is_live_peeks_without_consuming() {
        let s = spec(false, 2);
        let token = register("conn", "qid-live", &s);
        assert!(is_live(&format!("q:{token}:0")));
        assert!(take(&format!("q:{token}:0")).is_some());
        assert!(!is_live(&format!("q:{token}:1")));
    }

    #[test]
    fn skip_button_declines() {
        let s = spec(false, 2);
        let token = register("conn", "qid", &s);
        let msg = with_buttons(RichMessage::info("x"), &token, &s, Lang::Es);
        assert_eq!(msg.buttons.last().unwrap().label, "Omitir");
        assert!(msg.buttons.iter().all(|b| b.id.len() <= 64));
        let t = take(&format!("q:{token}:x")).unwrap();
        assert!(t.answer.declined && t.answer.answers.is_empty());
    }
}
