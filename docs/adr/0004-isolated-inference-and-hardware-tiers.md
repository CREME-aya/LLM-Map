# ADR-0004: 別プロセス推論隔離と GPU/NPU Tier 動的切替

- **ステータス**: Accepted
- **日付**: 2026-11-20
- **関連**: [ADR-0007](0007-l3-one-line-command-and-confirm-separation.md) / [ADR-0008](0008-dc-tick-single-event-entry.md)

## コンテキスト
走行中にローカル SLM を活用したい一方、ミドルレンジ Android 端末（RAM 6〜8 GB）では大規模モデル（3B〜4B）の同時稼働は OOM・発熱を招きやすく、さらに Android 端末の GPU (Vulkan/OpenCL) や NPU ドライバは機種差異によるネイティブクラッシュやハングのリスクが高い。また、純粋な経路採点には GPU/NPU は不要であり、推論エンジンだけを切り替える必要がある。

## 決定
1. **別プロセス隔離 (`:inference`)**: SLM 推論バックエンド (`SlmEngine`) を非公開バインドサービス（`android:process=":inference"`, `exported=false`）へ隔離し、地図・Valhalla・TTS・`ManeuverHorizon` を司るナビ本体プロセスと分離する。
2. **Tier 0 / 1 / 2 動的切替と保護**:
   - **Tier 0**: 決定論ロジックのみ（低 RAM・ADPF `SEVERE` 以上・障害発生時）。
   - **Tier 1（ミドル機既定）**: `llama.cpp` C API (CPU NEON) ＋ `Qwen2.5-0.5B-Instruct` Q4_K_M ＋ GBNF（出力 ≤ 32 tokens、2.5 秒打切り）。
   - **Tier 2（ハイエンド）**: 停車中の 10 秒マイクロセルフテストと実測性能条件（p95 20% 改善等）を満たした検証済み環境のみ `llama.cpp` Vulkan GPU（Phase 2 で NPU 向け `LiteRT-LM` 別アダプター追加）を有効化。
   - **保護機構**: 起動前ディスクマーカー＋失敗組合せブラックリスト化＋ADPF Thermal 段階縮退（走行中は降格のみ）。

## 検討した代替案
- **ナビと同一プロセス内で GPU/NPU 推論を実行する案**: GPU ドライバのネイティブ abort 発生時に走行中ナビ画面と右左折案内音声まで同時停止してしまうため不採用。
- **ミドル機でも常時 1.5B〜4B クラスをロードする案**: アプリ全体 PSS 目標（P95 ≤ 1.3 GB / 最大 ≤ 1.5 GB）を超過するため不採用。

## 影響とトレードオフ
- **利点**: 推論プロセスの異常終了・メモリ超過時にもナビ本体が生存し、即座に Tier 0 として運転案内を継続できる。
- **制約**: Binder 通信（粗粒度 JSON 命令のみ）の実装と、実機 2 台による ADPF・PSS・GPU 安定性計測（Gate 4 & 5）が必須になる。

## 再評価条件
- PoC Gate 4/5 で `Qwen2.5-0.5B` の日本語命令精度が目標（≥ 90%）に届かない場合は、まず語彙・候補 ID の絞り込みを行い、それでも不足する場合に限り停車中限定の 1.5B 別評価や `LiteRT-LM` 比較 PoC へ進む。
