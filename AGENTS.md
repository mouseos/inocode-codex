# InoCode 開発ルール

## プロジェクト外には書き込まない

- 趣旨: ビルドなど本来プロジェクト内でできることを、許可なくプロジェクト外へ置かない。
- ログ、一時ファイル、ビルド成果物、キャッシュ(`CARGO_HOME` を含む)は、プロジェクト内の無視された場所(`.build`、`codex-rs/target-win`)に置く。
  `D:\build` は使わない。`~/.cargo`・`~/.rustup`・npm のキャッシュなどのユーザー領域には、許可なく書かない。
- 作業の実行(ビルド、テスト、app-server やモックサーバーの起動、ポートの利用)は許可を求めずに進める。
- 許可なく書いてしまったら、隠さず報告し、自分が作ったものだけを片付ける。

## 作業の進め方: ブランチを分け、実装を書き切り、ローカル Windows だけで確認する

長いビルドやテストの完了を待って開発を止めない。CI(GitHub Actions)は使わない。

1. 1つの機能・修正ごとにブランチを切る。
2. **そのブランチの実装を最後まで書き切る。** 途中の編集ごとにビルドやテストを走らせない。
3. ビルドとテストはローカルの Windows だけで、速度を優先し、変更に関係する分だけ走らせる(`--profile dev-small`)。
   出力先はプロジェクト内の無視された場所に限る。ポーリングや sleep での待機はしない。
4. 失敗したものだけを直す。
5. 通ったブランチから `main` へマージする。

## ビルドの種類と置き場所

用途ごとにプロファイルを選ぶ。迷ったら `dev-small`。

- 静的検査: `cargo check --profile dev-small -p <crate>`。`--profile` を省くと `dev` 用の成果物が別に増える。
- 日常の実行・テスト: `cargo build|test --profile dev-small -p <crate>`。debug 情報なし、最適化なし。
- 配布・速度の確認: `cargo build --release`(`scripts/build-win.ps1` は dev-small のまま)。
- デバッガで中を追うとき: プロファイルを増やさず `CARGO_PROFILE_DEV_SMALL_DEBUG=full` を付ける。
- スタックや時間の内訳を見るとき: `cargo build --profile profiling`。

`dev-small` は `incremental = false`。このワークスペースでは増分が数十 GB を書き、短縮は小さい。
増分が要るときだけ `CARGO_INCREMENTAL=1` を付ける。

成果物は `codex-rs/target-win` の1か所だけ(`.cargo/config.toml` の `build.target-dir`)。
`CARGO_TARGET_DIR` を別の場所に変えない。`cargo test --workspace` や `cargo clean` の後の
全ビルドは16分以上かかるので、まず変更した crate だけを対象にする。

## 制約

- `gpt-6-astra` は使わない。API を実際に呼ぶ試験が必要なら `gpt-5.6-luna` のみを、モデル名を固定して使う。
- 他の OS でビルドできなくなる変更はしない。
- OAuth の外部照合値(クライアント ID、originator、バックエンド URL)は変更しない。
- 競合排除の確認として `scripts/check_isolation.py` を通す。
