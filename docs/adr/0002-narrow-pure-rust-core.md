# ADR-0002: 狭い純粋 Rust コアと AndroidX SQLite 統一

- **ステータス**: Accepted
- **日付**: 2026-11-20
- **関連**: [ADR-0007](0007-l3-one-line-command-and-confirm-separation.md) / [ADR-0008](0008-dc-tick-single-event-entry.md)

## コンテキスト
Android（Kotlin）、経路エンジン（Valhalla C++）、SLM 推論（`llama.cpp` C/C++）、Web 補完版（TypeScript）、全国データ生成パイプラインの間で、運転しやすさ採点・検証ロジック・状態機械をどう配置・共有するかが最大のアーキテクチャ分岐となった。一時は Kotlin Multiplatform (KMP) への一本化案も比較検討された。

## 決定
**IO・DB・HTTP・非同期・独自スレッドを一切持たない「狭い純粋 Rust コア」** を採用し、以下の境界で役割を固定する。

1. **Rust コアの責務**: L1/L2 3 軸採点、Intent Contract / `OverrideValidator`、旅判定スコア、`ManeuverHorizon` 正規化、音声スケジューラ、GPX リプレイ決定論テスト。
2. **Android との接続**: `cargo-ndk` による arm64 `cdylib` と **薄い手書き C ABI / JNI（粗粒度 ≤ 15 関数・版付き UTF-8 JSON）** を基本とし、Valhalla C++ や `llama.cpp` は Rust を通さず Kotlin から直接 JNI 呼び出しする（Rust ↔ C++ 直結禁止）。
3. **SQLite の統一**: AndroidX **`sqlite-bundled` (`BundledSQLiteDriver`)** のビルドフラグに `ENABLE_FTS5` と `ENABLE_RTREE` の両方が含まれることが一次ソースで確認できたため、SQLite は Kotlin/AndroidX 側へ統一し、Rust 側に `rusqlite` を持たせない。
4. **Web との共有**: JSON Schema (`schema/`) を唯一の正とし、Web (Zod) と Rust (`serde`) へ適用して共通フィクスチャで適合テストを行う。

## 検討した代替案
- **Kotlin Multiplatform (KMP) 一本化案**: Web 側が React + TypeScript + Zod（AI コーディングのデファクト）に確定したこと、Valhalla と `llama.cpp` のため NDK ビルド自体はどのみち不可欠であること、全国 OSM パイプライン処理でのメモリ効率・Rust コンパイラの型検査の利点から、KMP 案は取り下げて Rust へ合流した。
- **初期から `UniFFI` ＋ `wasm-bindgen` をフル導入する案**: JNA AAR の 16 KB 検査や初期ツールチェーン増加を抑えるため、Phase 1 は薄い手書き C ABI / JNI ＋ JSON Schema 適合テストから開始する。

## 影響とトレードオフ
- **利点**: Rust 側に永続化・ネットワークを持ち込まないことで `libsqlite3` / `libc++_shared` シンボル衝突を根絶でき、Mac ローカルでの `cargo test` 高速リプレイ検証と全国パイプラインでの採点再利用を両立できる。
- **制約**: 境界を増やしすぎない規律（粗粒度 JSON 契約の維持）が必要。

## 再評価条件
- Rust ↔ Kotlin 間の関数数や非同期コールバックが増加し手書き JNI の保守コストが上回った場合は `UniFFI` へ移行し、Web 側でブラウザ内採点が必須になった段階で検証済み純粋ロジックに限り `wasm-bindgen` アダプターを追加する。
