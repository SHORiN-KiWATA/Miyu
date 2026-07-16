use anyhow::{bail, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

const MAX_QUESTIONS: usize = 8;
const MAX_OPTIONS: usize = 8;
const MAX_HEADER_CHARS: usize = 30;
const MAX_QUESTION_CHARS: usize = 1_000;
const MAX_LABEL_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 400;
pub const MAX_CUSTOM_ANSWER_CHARS: usize = 4_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionPrompt {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default = "default_true")]
    pub custom: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequest {
    pub questions: Vec<QuestionPrompt>,
}

pub type QuestionAnswers = Vec<Vec<String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionResponse {
    Answered(QuestionAnswers),
    Cancelled,
    Unavailable(String),
}

#[derive(Debug)]
pub struct QuestionCancelled;

impl fmt::Display for QuestionCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("question cancelled by user")
    }
}

impl Error for QuestionCancelled {}

pub fn is_question_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<QuestionCancelled>().is_some()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct QuestionExchange {
    pub questions: Vec<QuestionPrompt>,
    pub answers: QuestionAnswers,
    pub answered_at: String,
}

impl QuestionExchange {
    pub fn new(request: QuestionRequest, answers: QuestionAnswers) -> Result<Self> {
        request.validate()?;
        validate_answers(&request, &answers)?;
        Ok(Self {
            questions: request.questions,
            answers,
            answered_at: Utc::now().to_rfc3339(),
        })
    }
}

impl QuestionRequest {
    pub fn parse(arguments: &str) -> Result<Self> {
        let request: Self = serde_json::from_str(arguments)?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        if self.questions.is_empty() {
            bail!("questions must contain at least one question");
        }
        if self.questions.len() > MAX_QUESTIONS {
            bail!("questions cannot contain more than {MAX_QUESTIONS} questions");
        }
        for (question_index, question) in self.questions.iter().enumerate() {
            validate_text(&question.header, "header", question_index, MAX_HEADER_CHARS)?;
            validate_text(
                &question.question,
                "question",
                question_index,
                MAX_QUESTION_CHARS,
            )?;
            if question.options.len() > MAX_OPTIONS {
                bail!(
                    "questions[{question_index}].options cannot contain more than {MAX_OPTIONS} options"
                );
            }
            if question.options.is_empty() && !question.custom {
                bail!(
                    "questions[{question_index}] must provide an option or allow a custom answer"
                );
            }
            let mut labels = BTreeSet::new();
            for (option_index, option) in question.options.iter().enumerate() {
                validate_text(
                    &option.label,
                    &format!("options[{option_index}].label"),
                    question_index,
                    MAX_LABEL_CHARS,
                )?;
                if option.description.chars().any(char::is_control) {
                    bail!(
                        "questions[{question_index}].options[{option_index}].description contains control characters"
                    );
                }
                if option.description.chars().count() > MAX_DESCRIPTION_CHARS {
                    bail!(
                        "questions[{question_index}].options[{option_index}].description is too long"
                    );
                }
                if !labels.insert(option.label.trim()) {
                    bail!("questions[{question_index}] contains duplicate option labels");
                }
            }
        }
        Ok(())
    }

    pub fn needs_review(&self) -> bool {
        self.questions.len() > 1 || self.questions.iter().any(|question| question.multiple)
    }
}

pub fn validate_answers(request: &QuestionRequest, answers: &QuestionAnswers) -> Result<()> {
    if answers.len() != request.questions.len() {
        bail!("answer count does not match question count");
    }
    for (index, (question, answer)) in request.questions.iter().zip(answers).enumerate() {
        if answer.is_empty() {
            bail!("question {index} is unanswered");
        }
        if !question.multiple && answer.len() != 1 {
            bail!("question {index} only accepts one answer");
        }
        let option_labels = question
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect::<BTreeSet<_>>();
        let mut unique = BTreeSet::new();
        for value in answer {
            let value = value.trim();
            if value.is_empty() {
                bail!("question {index} contains an empty answer");
            }
            if value.chars().count() > MAX_CUSTOM_ANSWER_CHARS {
                bail!("question {index} contains an answer that is too long");
            }
            if !option_labels.contains(value) && !question.custom {
                bail!("question {index} does not allow custom answers");
            }
            if !unique.insert(value) {
                bail!("question {index} contains duplicate answers");
            }
        }
    }
    Ok(())
}

pub fn answered_tool_output(exchange: &QuestionExchange) -> String {
    let answers = exchange
        .questions
        .iter()
        .zip(&exchange.answers)
        .map(|(question, selected)| {
            json!({
                "header": question.header,
                "question": question.question,
                "answers": selected,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({
        "status": "answered",
        "answers": answers,
        "instruction": "Continue using these user-provided answers. Do not ask the same questions again.",
    }))
    .unwrap_or_else(|_| "{\"status\":\"answered\"}".to_string())
}

pub fn unavailable_tool_output(reason: &str) -> String {
    serde_json::to_string(&json!({
        "status": "unavailable",
        "reason": reason,
        "instruction": "Interactive input is unavailable. Continue safely without assuming an answer.",
    }))
    .unwrap_or_else(|_| "{\"status\":\"unavailable\"}".to_string())
}

pub fn assistant_exchange_text(exchange: &QuestionExchange) -> String {
    let mut output = String::from("补充确认：");
    for (index, question) in exchange.questions.iter().enumerate() {
        output.push_str(&format!(
            "\n{}. [{}] {}",
            index + 1,
            question.header,
            question.question.trim()
        ));
        for option in &question.options {
            output.push_str(&format!("\n   - {}", option.label));
            if !option.description.is_empty() {
                output.push_str(&format!(": {}", option.description));
            }
        }
        if question.custom {
            output.push_str("\n   - 可输入自定义答案");
        }
    }
    output
}

pub fn user_exchange_text(exchange: &QuestionExchange) -> String {
    let mut output = String::from("补充回答：");
    for (question, answers) in exchange.questions.iter().zip(&exchange.answers) {
        output.push_str(&format!("\n- {}：{}", question.header, answers.join("、")));
    }
    output
}

fn validate_text(value: &str, field: &str, question_index: usize, max_chars: usize) -> Result<()> {
    if value.trim().is_empty() {
        bail!("questions[{question_index}].{field} cannot be empty");
    }
    if value.trim() != value {
        bail!("questions[{question_index}].{field} cannot have surrounding whitespace");
    }
    if value.chars().any(char::is_control) {
        bail!("questions[{question_index}].{field} contains control characters");
    }
    if value.chars().count() > max_chars {
        bail!("questions[{question_index}].{field} is too long");
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![QuestionPrompt {
                header: "范围".to_string(),
                question: "修改哪些文件？".to_string(),
                options: vec![QuestionOption {
                    label: "全部".to_string(),
                    description: "修改全部相关文件".to_string(),
                }],
                multiple: false,
                custom: true,
            }],
        }
    }

    #[test]
    fn request_accepts_option_and_custom_answers() {
        let request = request();
        assert!(validate_answers(&request, &vec![vec!["全部".to_string()]]).is_ok());
        assert!(validate_answers(&request, &vec![vec!["仅配置".to_string()]]).is_ok());
    }

    #[test]
    fn single_question_rejects_multiple_answers() {
        let request = request();
        assert!(validate_answers(
            &request,
            &vec![vec!["全部".to_string(), "仅配置".to_string()]]
        )
        .is_err());
    }

    #[test]
    fn request_rejects_duplicate_labels() {
        let mut request = request();
        let duplicate = request.questions[0].options[0].clone();
        request.questions[0].options.push(duplicate);
        assert!(request.validate().is_err());
    }

    #[test]
    fn request_rejects_terminal_control_sequences() {
        let mut request = request();
        request.questions[0].question = "选择\u{1b}[2J范围".to_string();
        assert!(request.validate().is_err());
    }

    #[test]
    fn request_rejects_surrounding_label_whitespace() {
        let mut request = request();
        request.questions[0].options[0].label = " 全部 ".to_string();
        assert!(request.validate().is_err());
    }

    #[test]
    fn persisted_assistant_exchange_keeps_option_meaning() {
        let exchange = QuestionExchange::new(request(), vec![vec!["全部".to_string()]]).unwrap();
        let text = assistant_exchange_text(&exchange);
        assert!(text.contains("全部: 修改全部相关文件"));
        assert!(text.contains("可输入自定义答案"));
    }
}
