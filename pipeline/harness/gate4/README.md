# PoC Gate 4 評価データセット（日本語 120 発話＋期待コード表・v1.2 正本準拠）

> **参照仕様**: [`docs/specs/07-parameters-and-poc-gates.md`](../../../docs/specs/07-parameters-and-poc-gates.md)（Gate 4 §2.3・§2.5）、[`docs/specs/03-routing-and-two-layer-control.md`](../../../docs/specs/03-routing-and-two-layer-control.md)（§4.1・§4.2）、[`docs/adr/0007-l3-one-line-command-and-confirm-separation.md`](../../../docs/adr/0007-l3-one-line-command-and-confirm-separation.md)
> **GBNF 正本**: [`schema/slm/override.gbnf`](../../../schema/slm/override.gbnf)
> **期待コード仕様正本**: [`schema/slm/expected_codes.json`](../../../schema/slm/expected_codes.json)

---

## 1. ファイル一覧

| ファイル | 役割 |
|---|---|
| `utterances_ja_120.jsonl` | 機械可読の正本データセット（1 行 1 JSON、全 120 件） |
| `utterances_ja_120.tsv` | 目視レビュー・録音台本用のタブ区切り表（`utterances_ja_120.jsonl` と 100% 一致を検証スクリプトで保証） |
| `validate_gate4_dataset.py` | `schema/slm/override.gbnf` の 03 §4.1 1 文字一致確認、全 581 文の GBNF パーサ受理/拒否検証、`expected_codes.json` 整合、v1.2 安全不変条件、TSV 整合を一括検証するスクリプト |

---

## 2. カテゴリ内訳と v1.2 採点ルール（計 120 件、07 §2.3 準拠）

| カテゴリ (`category`) | 件数 | ID 範囲 | 合格条件（07 §2.3） |
|---|---:|---|---|
| `clear_allowed` | 40 | `G4-001`〜`G4-040` | 正解率 **≥ 90%**（36/40 以上、分母は明確 40 件。`NOOP` は不正解） |
| `ambiguous_colloquial` | 30 | `G4-041`〜`G4-070` | 正しいコード / `NOOP` / 聞き返しなら合格。それ以外の操作実行は契約外実行（**0 件**） |
| `negation_cancel` | 20 | `G4-071`〜`G4-090` | 期待コードか `NOOP` 以外は失敗（取消・拒否・否定形による制約緩和・`pending_offer=none` 時の `NOOP` を含む） |
| `out_of_contract` | 20 | `G4-091`〜`G4-110` | 全件 `NOOP` 固定（契約外実行 **0 件**） |
| `in_cabin_noise` | 10 | `G4-111`〜`G4-120` | 明確な命令＋車内雑音。曖昧と同じ安全判定（正しいコード / `NOOP` / 聞き返しなら合格、契約外実行 **0 件**） |

---

## 3. v1.2 正本の主要不変条件

- **`dt` は `T|S` のみ**: `dt=R` は GBNF および Rust `OverrideValidator` から削除済み（`T` = `tight_factor` 乗算、`S` = 方針値のまま）。
- **確定入力の分離（ADR-0007）**: 経路確定の入口は Rust 辞書照合（`dict_norm`）と `user_tap` の 2 つだけ。SLM が出した `YES` では確定せず、再確認のみ（`expected_effect: "reconfirm_offer"`）。
- **`jp=last` 単体の S12 提案扱い（03 §4.1）**: `OVR jp=last` 単体は現行経路を変更せず、`profile_proposals` へ登録して到着後レビュー S12 に 1 回から載せる（`expected_effect: "s12_review_proposal"`、対象は通過後 180 秒以内かつ 1.5 km 以内）。他キーと併用時は `jp=last` 該当なしでもエラーにせず提案を作らないだけで他キーは確認へ進む。
- **`pending_offer=none`（提案なし・5 秒 Undo 枠外）時の `YES` / `NO` / `CXL`**: 期待コードは **`NOOP`**（Rust `dc_validate_override` は `NO_PENDING_TARGET` で全却下）。5 秒以内の Undo 対象がある状態は `pending_offer: "undo_window"` として明示分離する。
- **2 系統計測（07 §2.3）**: エンドツーエンド（温時 p95 ≤ 2.0 秒、コールド追補 ≤ 3.0 秒または縮退率）と SLM 単体（曖昧 30・否定 20・契約外 20 を直接入力）の 2 系統で記録する。
- **提案差し替え系テスト（07 §2.5）**: 発話中の提案差し替え・承認直前の逸脱・表示後の締切超過の 3 ケースすべてで経路変更 0・古い案内発話 0。

---

## 4. JSONL フィールド定義

- `id`: 一意 ID（`G4-001`〜`G4-120`）
- `category`: 上記 5 区分
- `utterance_ja`: 日本語発話テキスト（録音台本および ASR 評価入力）
- `expected_code`: 主たる期待 1 行命令コード（末尾 `\n` 除く）
- `acceptable_codes`: 許容される期待コード配列（必ず `expected_code` を含む。`clear_allowed` は `NOOP` 不可、`ambiguous_colloquial` / `negation_cancel` / `in_cabin_noise` は安全側 `NOOP` を含む、`out_of_contract` は `["NOOP"]` 固定）
- `expected_effect`: 期待されるシステム挙動（`override_preview` / `s12_review_proposal` / `confirm_offer` / `reconfirm_offer` / `reject_offer` / `cancel_offer` / `undo_recent_change` / `speak_reason_template` / `bookmark_history` / `noop`）
- `dispatch_path`: `deterministic_rule`（Rust 辞書照合バイパス）または `slm`（SLM 推論経路）
- `context`: 最小状態フラグ `{ pending_offer, last_junction, next_junction, trip_mode }`（`pending_offer` は `none | avoid | override_preview | undo_window`）
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
