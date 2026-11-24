# 共通基盤・ビルド環境・統合検証スクリプト (`scripts/`)

本ディレクトリは、各領域（`docs/`・`schema/`・`core-rust/`・`android/`・`web/`・`pipeline/`）の成果物を安全に統合・検証するための共通基盤および Gate 0/1 向けチェックツールを提供します。

## 1. 外付け SSD (`/Volumes/suneast`) マウントガード & 環境変数設定

内蔵 SSD の空き容量が逼迫しているため、ビルドキャッシュおよびツール出力は `/Volumes/suneast/dev` 配下へ集約します。未マウント時の誤書き込みを防ぐため、必ずガード付きスクリプト経由で環境変数を読み込みます。

```sh
# 初回のみディレクトリ作成（明示実行）
bash scripts/env-suneast.sh --init-dirs

# シェルへ環境変数を反映
source scripts/env-suneast.sh

# 出力内容のみ確認
bash scripts/env-suneast.sh --print
```

## 2. Gate 0 ビルド環境監査 (`scripts/check-env.sh`)

```sh
make check-env
# JSON 形式で取得する場合
make check-env-json
# Gate 0 必須項目が不足している場合に非ゼロ終了
bash scripts/check-env.sh --strict-gate0
```

### ローカル Mac 実査で見つかった Gate 0 残課題
- 外付け SSD `/Volumes/suneast`（Case-sensitive APFS・空き 426 GiB）はマウント済みだが、`/Volumes/suneast/dev` 未初期化および `CARGO_TARGET_DIR` / `GRADLE_USER_HOME` / `ANDROID_HOME` 等の環境変数が未設定。
- Android SDK は `~/Library/Android/sdk` に存在（`android-36`, `build-tools/35.0.0 & 36.1.0`, `zipalign`, `system-images/android-36.1/google_apis_playstore/arm64-v8a`, AVD `Medium_Phone_API_36.1`）だが、**Android NDK (`ndk/`) が未導入**。
- Rust は `rustc/cargo 1.96.0` が導入済みだが、**`aarch64-linux-android` ターゲットおよび `cargo-ndk` が未導入**。
- ネイティブビルド用の **`cmake` / `ninja` / `bundletool`** が未導入。
- `git remote` が未設定（PR 作成前にリモート登録が必要）。

## 3. 16 KB ページアラインメント検査 (`scripts/check-16k-elf.sh`)

Android 16 KB ページ要件（`arm64-v8a` 全 `.so` の ELF `PT_LOAD` `p_align >= 16384`）を、macOS・Linux 共通で外部ツール不要の Python ELF64 パーサにより検証します。APK 検査時は `zipalign -c -P 16 -v 4` も自動併用します。

```sh
make check-16k TARGET=path/to/libdrive_core.so
bash scripts/check-16k-elf.sh --abi arm64-v8a --check-zip-offset path/to/app-release.apk
```

## 4. 全体統合検証 (`scripts/verify-integration.sh`)

各コンポーネントがマージされる前後を問わず、単一コマンドでリポジトリ全体の健全性と各領域のテストを実行します。

```sh
make verify
# コアコンポーネント（docs/・schema/・core-rust/・pipeline/）揃った後の厳格チェック
bash scripts/verify-integration.sh --require-components
# Web 補完版（web/）も必須にする場合
bash scripts/verify-integration.sh --require-components --require-web
```

検証項目:
1. Case-sensitive APFS パス衝突検査（大文字小文字のみ異なるパスの禁止）
2. 全 JSON ファイル構文検証
3. 相対 Markdown リンク実在検証
4. シークレット直書き簡易スキャン
5. `scripts/*.sh` 構文チェック（`bash -n`）およびスクリプト単体テスト（`scripts/tests/`）
6. 存在するコンポーネントごとの自動検証（`docs/`・`schema/`・`core-rust/`・`web/`・`pipeline/`（各サブディレクトリの `test_*.py` および Gate 4 検証））
