# Upstream

InoCode は openai/codex (Apache-2.0) のスナップショットから独立して開発する。upstream へは PR を出さず、fork 関係も持たない。

- 取得元: https://github.com/openai/codex
- 取得コミット: 5b98eaafeb341225d3724a6112a93c993cc09613
- LICENSE / NOTICE は Apache-2.0 の要件のため保持する。

## 上流との差分の方針
- 新機能は `codex-rs/ext/` の新しいクレート(`inocode-*`)に置く。`core`、`app-server`、`tui` など既存のクレートへの変更は、
  クレートを呼ぶ最小の接続点(登録、match の腕、設定の読み取り)に限る。
- ビルドと試験は、上流の手順(cargo、`justfile`、Bazel)をそのまま使う。自作スクリプト(`scripts/ino-env.*`、`scripts/ino-clean.ps1`)は、
  環境変数の設定と成果物の掃除だけの薄いラッパーで、cargo の引数やプロファイルは変えない。
- 整形だけの変更、意味のない rename は入れない。上流のコードの書き方(clippy の deny、`format!` の書式など)に合わせる。

## 現在の主な差分(基準 44231e2 から HEAD、2,364 ファイル。2026-11-13 時点)
| 領域 | 内容 |
|---|---|
| 全体の一括置換(926 ファイル) | `CODEX_*` → `INOCODE_*`、`.codex` → `.inocode`、ユーザーに見える名前を InoCode に(be2d276、af83ce0、0b6c678)。`tui` は 571 ファイル中 566 が置換のみ |
| 隔離(9cdc1ea) | keyring、Windows sandbox、WFP GUID、ログインのポート、updater、バイナリ名 `inocode` |
| `lib/` | `zero-search`(組み込みの検索ライブラリ、281 ファイル) |
| `codex-rs/ext/` の新クレート | inocode-knowledge / evidence / metrics / advisor / team / protocol / flags / stats / deepai / model-catalog / tool-output、zero-search。`ext/web-search` は削除、`ext/goal` は固定性の修正 |
| `core` | 複数エージェント(wait、message_tool、spawn)、圧縮、exec policy(強制削除のスコープ)、unified_exec(端末の監視)、ツール登録(ZeroSearch 置換)、snapshot の更新 |
| `app-server` / `app-server-protocol` | `inocode/*` の拡張メソッド、アカウント、統計、モデル一覧。スキーマ固定ファイルの再生成(144 ファイル追加) |
| `login` / `model-provider` / `codex-api` | アカウントプール、Claude と DeepAI のプロバイダ、リセット券の履歴 |
| `tui` | 置換のほか、通知の網羅 match への腕(5 ファイル) |
| `.github` | 上流のリリース・bot のワークフローを削除(31 ファイル、a5de82b)。InoCode のテスト用ワークフローだけ残す |
| `sdk` | 生成された Python 型の更新(2 ファイル) |

### 差分の再確認の結果(一覧だけ。削除はしていない)
- 整形だけの変更: 見つからなかった(`git diff -w` で空になるファイルなし)。
- 置換だけのファイル(置換のコミットのみが触った): 1,471。領域別の内訳は `ref/docs/66-upstream-diff-audit.md`。
- 置換以外の変更があるファイル(240 件。追加と削除を除く)は、確認できた範囲(tui、sdk、utils、protocol、exec、config、login、rollout、shell-command、codex-api、model-provider)で、機能のコミット(アカウント、検索、goal、端末の監視、rollout のメモリ予算、強制削除など)に対応。
  core と app-server-protocol の 160 件は、コミット件名の確認まで行っていない(ファイル名の一覧のみ)。無関係な変更は見つかっていない。
- 上流への影響が大きい点(見直す余地): `.github` の削除 31 ファイル、`core` の snapshot 約 50 ファイルの更新(名前の置換と web search の削除に由来)。
