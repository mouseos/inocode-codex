use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Rule,
    FailureCase,
    Issue,
    Decision,
    Fact,
    Contract,
    Procedure,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Rule,
        Kind::FailureCase,
        Kind::Issue,
        Kind::Decision,
        Kind::Fact,
        Kind::Contract,
        Kind::Procedure,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Rule => "rule",
            Kind::FailureCase => "failure_case",
            Kind::Issue => "issue",
            Kind::Decision => "decision",
            Kind::Fact => "fact",
            Kind::Contract => "contract",
            Kind::Procedure => "procedure",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "project" => Some(Scope::Project),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Unverified,
    Verified,
    Superseded,
    Retracted,
}

impl Status {
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "unverified" => Some(Status::Unverified),
            "verified" => Some(Status::Verified),
            "superseded" => Some(Status::Superseded),
            "retracted" => Some(Status::Retracted),
            _ => None,
        }
    }
    /// 置き換え・撤回されたものは、禁止事項や引き継ぎには使わない。
    pub fn is_live(self) -> bool {
        !matches!(self, Status::Superseded | Status::Retracted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Block,
    Warn,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// ツール名の glob(例 `apply_patch`、`Write`、`mcp__*`)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// MCP の `server` または `server/tool`(glob 可)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    /// モデル名(スラッグ)の glob。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 書き込む内容(ファイルの書き込み・編集・パッチの本文)に対する正規表現。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<String>,
    /// これに合うパスを触る操作は、この規則の対象外にする(glob)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_paths: Vec<String>,
}

impl MatchSpec {
    /// 照合の条件を 1 つでも持つか(`exclude_paths` だけでは条件にならない)。
    pub fn has_criteria(&self) -> bool {
        !(self.paths.is_empty()
            && self.commands.is_empty()
            && self.tools.is_empty()
            && self.mcp.is_empty()
            && self.models.is_empty()
            && self.content.is_empty())
    }
}

/// front-matter + 本文。`id` が空なら書き込み時に採番する。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub id: String,
    pub kind: Kind,
    pub title: String,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default = "default_status")]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    // rule
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_spec: Option<MatchSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_failure: Option<String>,
    /// 対象を絞るパス(glob。絶対でも `gui/inocode-desktop` のような相対でもよい)。
    /// セッションの cwd かこの操作のパスがどれかに合うときだけ効く。空なら scope の範囲全体。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    // failure_case
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symptom: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_not: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what_worked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<String>>,
    /// 何のプロジェクトか(リポジトリのパスとブランチ)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 何をしようとしていたか(そのターンのユーザーの依頼の先頭 200 字)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// 何を実行したか(コマンドと引数の全文。長ければ先頭と末尾。cwd)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempted: Option<String>,
    /// どうなったか(終了コード、出力の末尾)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// 失敗した時点の条件(直前のコマンド、cwd、終了コード、出力の末尾、使えたツール)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<String>,
    /// 原因(分かるとき)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// 同じ呼び出しが別の状態では成功した: 呼び出し自体の禁止ではなく、状態の条件がある失敗。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dependent: Option<bool>,
    /// 成功したときの条件(`state_dependent` のとき)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub works_when: Option<String>,
    /// Markdown 本文(front-matter の外)。
    #[serde(default)]
    pub body: String,
}

/// What a failure case records about the failed call besides its signature and symptom.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureDetail {
    pub project: Option<String>,
    pub purpose: Option<String>,
    pub attempted: Option<String>,
    pub outcome: Option<String>,
    pub conditions: Option<String>,
    pub cause: Option<String>,
}

fn default_scope() -> Scope {
    Scope::Project
}
fn default_status() -> Status {
    Status::Unverified
}

impl Record {
    pub fn new(kind: Kind, title: &str) -> Self {
        Record {
            id: String::new(),
            kind,
            title: title.to_string(),
            scope: Scope::Project,
            status: Status::Unverified,
            source: None,
            confidence: None,
            created_at: String::new(),
            updated_at: String::new(),
            created_by: None,
            supersedes: None,
            severity: None,
            match_spec: None,
            reason: None,
            enforced_by: None,
            from_failure: None,
            applies_to: Vec::new(),
            signature: None,
            symptom: None,
            root_cause: None,
            do_not: None,
            what_worked: None,
            evidence: None,
            project: None,
            purpose: None,
            attempted: None,
            outcome: None,
            conditions: None,
            cause: None,
            state_dependent: None,
            works_when: None,
            body: String::new(),
        }
    }

    pub fn to_file_text(&self) -> String {
        let mut front = self.clone();
        front.body = String::new();
        // body は front-matter に入れない
        let mut value = serde_yaml::to_value(&front).unwrap_or(serde_yaml::Value::Null);
        if let serde_yaml::Value::Mapping(m) = &mut value {
            m.remove(serde_yaml::Value::String("body".into()));
        }
        let yaml = serde_yaml::to_string(&value).unwrap_or_default();
        format!("---\n{yaml}---\n{}", self.body)
    }

    pub fn from_file_text(text: &str) -> Option<Record> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let text = text.replace("\r\n", "\n");
        let rest = text.strip_prefix("---\n")?;
        let end = rest.find("\n---")?;
        let yaml = &rest[..end];
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after);
        let mut rec: Record = serde_yaml::from_str(yaml).ok()?;
        rec.body = body.to_string();
        Some(rec)
    }

    /// 一覧用の 1 行要約。
    pub fn summary(&self) -> String {
        let first = self
            .reason
            .as_deref()
            .or(self.symptom.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                self.body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            });
        first.chars().take(160).collect()
    }
}
