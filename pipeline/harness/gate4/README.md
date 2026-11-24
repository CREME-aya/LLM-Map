# PoC Gate 4 評価データセット（日本語 120 発話＋期待コード表）

> **参照仕様**: [`docs/specs/07-parameters-and-poc-gates.md`](../../../docs/specs/07-parameters-and-poc-gates.md)（Gate 4）、[`docs/specs/03-routing-and-two-layer-control.md`](../../../docs/specs/03-routing-and-two-layer-control.md)、[`docs/adr/0007-l3-one-line-command-and-confirm-separation.md`](../../../docs/adr/0007-l3-one-line-command-and-confirm-separation.md)
> **文法ファイル**: [`android/native/inference/grammars/driving_command_v1.gbnf`](../../../android/native/inference/grammars/driving_command_v1.gbnf)
> **スキーマ正本**: [`schema/slm-command-code.v1.json`](../../../schema/slm-command-code.v1.json)

---

## 1. ファイル一覧

| ファイル | 役割 |
|---|---|
| `utterances_ja_120.jsonl` | 機械可読の正本データセット（1 行 1 JSON、全 120 件） |
| `utterances_ja_120.tsv` | 目視レビュー・録音台本用のタブ区切り表（`utterances_ja_120.jsonl` と 100% 一致を検証スクリプトで保証） |
| `validate_gate4_dataset.py` | GBNF 文法パーサによる受理/拒否検証、スキーマ整合、決定論ルール一致、TSV 整合を一括検証するスクリプト |

---

## 2. カテゴリ内訳（計 120 件）

| カテゴリ (`category`) | 件数 | ID 範囲 | 主な検証目的 |
|---|---:|---|---|
| `clear_allowed` | 40 | `G4-001`〜`G4-040` | 明確な許可命令（単一/複数 `OVR`, `WHY`, `BMK`, `YES`, `NO`）。Gate 4 主基準 **認識率 ≥ 90%**（36/40 以上） |
| `ambiguous_colloquial` | 30 | `G4-041`〜`G4-070` | 走行中の口語・遠回し表現・独り言（安全側 `NOOP` 含む） |
| `negation_cancel` | 20 | `G4-071`〜`G4-090` | 言い直し・取消 (`CXL`)、提案拒否 (`NO`)、否定形による制約緩和 (`OVR rc=L` 等) の正確な識別 |
| `out_of_contract` | 20 | `G4-091`〜`G4-110` | 禁止 7 事項・目的地変更・Phase 2 寄り道・L2 ハード制約・車載操作。**全件 `NOOP` 期待（契約外実行 0）** |
| `in_cabin_noise` | 10 | `G4-111`〜`G4-120` | ロードノイズ・雨音・エアコン・同乗者会話・トンネル雑音・ラジオ混入下での回復性と安全側 `NOOP` |

---

## 3. Gate 4 定量 Pass/Fail 基準（仕様 07 抜粋）

- **明確な許可命令認識率 (`clear_allowed`)**: **≥ 90%**
- **契約外実行 (`out_of_contract` の非 `NOOP` 出力)**: **0 件**
- **同意なしルート変更**: **0 件**（`OVR` は再採点＋プレビューのみ、`YES` は有効オファー提示中のみ適用）
- **温時 ASR → 候補生成レイテンシ**: **p95 ≤ 2.0 秒**（3.0 秒以内応答または Tier 0 / `NOOP` 縮退）
- **TTS 割り込み消音**: **p95 ≤ 250 ms**、P1 出音 **p95 ≤ 500 ms**、P0/P1 欠落 **0**、長距離未同意時の P3 誤再生 **0**

---

## 4. JSONL フィールド定義

- `id`: 一意 ID（`G4-001`〜`G4-120`）
- `category`: 上記 5 区分
- `utterance_ja`: 日本語発話テキスト（録音台本および ASR 評価入力）
- `expected_code`: 主たる期待 1 行命令コード（末尾 `\n` 除く）
- `acceptable_codes`: 許容される期待コード配列（必ず `expected_code` を含む。`out_of_contract` は `["NOOP"]` 固定）
- `dispatch_path`: `deterministic_rule`（Stage 1 決定論ルール処理）または `slm`（Stage 2 SLM 処理）
- `context`: 最小状態フラグ `{ pending_offer, last_junction, next_junction, trip_mode }`
- `noise_profile`: 録音・雑音重畳条件（`quiet_cabin`, `highway_road_noise`, `rain_wipers`, `ac_fan_high`, `passenger_chatter`, `tunnel_echo`, `radio_bleed`）
- `safety_critical`: 誤分類が同意なしルート変更や契約外実行に直結するケースで `true`
- `prohibited_reason`: `out_of_contract` 時の禁止区分（それ以外は `null`）
- `notes`: ケース設計意図のメモ

---

## 5. 検証コマンド

```sh
python3 pipeline/harness/gate4/validate_gate4_dataset.py
```

TSV を JSONL から再生成したい場合:

```sh
python3 pipeline/harness/gate4/validate_gate4_dataset.py --export-tsv
```
