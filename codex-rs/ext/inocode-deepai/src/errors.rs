//! The errors DeepAI returns, and what the user is told.
//!
//! The server wraps most errors as `{"error": {"message", "type", "code"}}`; the code exchange
//! answers with the flat `{"error": "..."}`. Both the HTTP status and `error.code` are kept:
//! the user-facing text always names them, so nothing is lost by the translation.

use serde_json::Value;

/// A failed answer, as received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepAiError {
    pub status: u16,
    /// `error.code`, when the body carried one.
    pub code: Option<String>,
    /// The server's own message, when the body carried one.
    pub message: Option<String>,
}

impl DeepAiError {
    /// Reads an error answer. An unreadable body is kept as no code and no message.
    pub fn from_response(status: u16, body: &str) -> Self {
        let value: Option<Value> = serde_json::from_str(body).ok();
        let error = value.as_ref().and_then(|v| v.get("error"));
        let (code, message) = match error {
            Some(Value::Object(map)) => (
                map.get("code").and_then(Value::as_str).map(str::to_string),
                map.get("message").and_then(Value::as_str).map(str::to_string),
            ),
            Some(Value::String(text)) => (None, Some(text.clone())),
            _ => (None, None),
        };
        Self {
            status,
            code,
            message,
        }
    }

    /// No answer was received at all (status 0).
    pub fn unreachable(reason: &str) -> Self {
        Self {
            status: 0,
            code: None,
            message: Some(reason.to_string()),
        }
    }

    /// The text shown to the user: Japanese, naming the code and the HTTP status.
    pub fn user_message(&self) -> String {
        if self.status == 0 {
            return format!(
                "DeepAI から答えを得られませんでした。{}",
                self.message
                    .as_deref()
                    .map(|reason| format!(" 理由: {reason}"))
                    .unwrap_or_default()
            );
        }
        let tag = match &self.code {
            Some(code) => format!("(HTTP {}, {code})", self.status),
            None => format!("(HTTP {})", self.status),
        };
        let text = match (self.code.as_deref(), self.status) {
            (Some("auth_required"), _) | (None, 401) => {
                "DeepAI にサインインしていないか、API キーが無効です。サインインし直してください。"
            }
            (Some("pro_required"), _) | (None, 402) => {
                "このモデルは DeepAI Pro の契約が必要です。"
            }
            (Some("lane_version_unsupported"), _) | (None, 426) => {
                "DeepAI が、この InoCode の接続方式の版数を受け付けません。InoCode を更新してください。"
            }
            (Some("social_login_required"), _) => {
                "このアカウントのログイン方法では使えません。DeepAI で Google か Apple によるサインインをしてください。"
            }
            (Some("theos_insufficient"), _) => {
                "このリクエストの Theos 消費量が、残高を超えています。"
            }
            (Some("email_unverified"), _) => {
                "メールアドレスの確認が済むまで Theos は付与されません。DeepAI でメールを確認してください。"
            }
            (Some("theos_exhausted"), _) => "今日の Theos の付与を使い切りました。",
            (Some("rate_limit_exceeded"), _) | (None, 429) => {
                "DeepAI の利用上限に達しました(日次などの上限)。"
            }
            (Some("machine_required"), _) => {
                "このマシンを識別できないため、Theos を消費するモデルは使えません。"
            }
            (Some("check_in_refused"), _) => {
                "DeepAI が、今日はこのアプリの接続(チェックイン)のやり直しが多すぎるとして、接続を断りました。キーの失効ではありません。日が変わってから、もう一度試してください。"
            }
            (Some("check_in_unavailable"), _) => {
                "DeepAI との接続の更新ができませんでした。もう一度試してください。"
            }
            (Some("session_busy"), _) => "DeepAI のセッションが混み合っています。順番を待っています。",
            _ => "DeepAI がエラーを返しました。",
        };
        match &self.message {
            Some(message) if !message.is_empty() => format!("{text} {tag} サーバーの文言: {message}"),
            _ => format!("{text} {tag}"),
        }
    }
}

impl std::fmt::Display for DeepAiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for DeepAiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapped_and_the_flat_shapes_are_both_read() {
        let wrapped = DeepAiError::from_response(
            401,
            r#"{"error":{"message":"Sign in to DeepAI to use DeepAI Code.","type":"auth_required","code":"auth_required"}}"#,
        );
        assert_eq!(wrapped.code.as_deref(), Some("auth_required"));
        let flat = DeepAiError::from_response(400, r#"{"error":"invalid or expired code"}"#);
        assert_eq!(flat.code, None);
        assert_eq!(flat.message.as_deref(), Some("invalid or expired code"));
        assert_eq!(DeepAiError::from_response(502, "<html>").code, None);
    }

    #[test]
    fn every_documented_code_has_a_japanese_text_that_keeps_code_and_status() {
        for (status, code) in [
            (401, "auth_required"),
            (402, "pro_required"),
            (426, "lane_version_unsupported"),
            (403, "social_login_required"),
            (402, "theos_insufficient"),
            (403, "email_unverified"),
            (429, "theos_exhausted"),
            (429, "rate_limit_exceeded"),
            (403, "machine_required"),
        ] {
            let error = DeepAiError {
                status,
                code: Some(code.to_string()),
                message: None,
            };
            let text = error.user_message();
            assert!(text.contains(code) && text.contains(&format!("HTTP {status}")), "{text}");
            assert!(!text.starts_with("DeepAI がエラーを返しました"), "{code} fell to the default");
        }
    }

    #[test]
    fn a_status_alone_is_enough_when_the_body_has_no_code() {
        let text = DeepAiError::from_response(429, "").user_message();
        assert!(text.contains("HTTP 429") && text.contains("上限"));
    }
}
