# ADR-0007: 走行中 L3 1行命令コードと確定入力の分離

- **ステータス**: Proposed
- **日付**: 2026-11-24
- **関連**: [ADR-0001](0001-two-layer-deterministic-routing-first.md) / [ADR-0002](0002-narrow-pure-rust-core.md) / [ADR-0004](0004-isolated-inference-and-hardware-tiers.md) / [ADR-0008](0008-dc-tick-single-event-entry.md) / [03 §1・§4.1・§4.2・§5](../specs/03-routing-and-two-layer-control.md) / [04 §2・§4.3](../specs/04-maneuver-horizon-and-hmi.md) / [05 §3.2・§4.1](../specs/05-inference-tier-and-audio.md) / [07 §2](../specs/07-parameters-and-poc-gates.md)

## コンテキスト
走行中の L3 は `:inference` の 0.5B SLM（出力 ≤ 32 tokens、[ADR-0004](0004-isolated-inference-and-hardware-tiers.md)）で動かすため、停車中と同じ JSON 契約をそのまま出させることはできない。v1.1 までの docs は走行中の出力を「GBNF による固定命令コード（例：`right_crossing=high` に紐づく閉じた列挙トークン）を Rust 側で同一型へ展開」とだけ書き、文法・コードの種類・参照先の決め方・却下理由を定めていなかった。

また、経路変更の「明示同意（「はい」等）」について、その発話を SLM が解釈した結果で確定してよいのか、決定論の照合だけで受けるのかが分けられていなかった。走行中は発話している間に提案が差し替わる、承認の直前に逸脱する、表示した後に締切を過ぎる、といった競合が起こり得る（Gate 4 / 7 の共通テスト項目）。

## 決定
1. **走行中 L3 の出力は 1 行命令コード 7 種に限定する**: `OVR` / `CXL` / `WHY` / `BMK` / `YES` / `NO` / `NOOP`。
2. **GBNF の正本はファイル `schema/slm/override.gbnf`**（全文の写しは [03 §4.1](../specs/03-routing-and-two-layer-control.md)。本 ADR には要点だけ書く）。
   - 先頭のコードで分岐し、`OVR` のキーは `rc` → `nr` → `mg` → `dt` → `jp` の固定順。
   - キーが 1 つもない `OVR` は文法上出せない。`rc` / `nr` / `mg` の値は `L|N|H`、`dt` は `T|S` のみ、`jp` は `last|next` のみ。
3. **解析は Rust の `dc_validate_override` で行う**。Kotlin は命令コードの行を JSON に組み直さず、`code_line` のまま渡す（入出力の型は [02 §2.1](../specs/02-architecture-and-stack.md)）。
4. **`jp` の参照先**（参照表の正本は `ManeuverHorizon.refs`、[04 §2](../specs/04-maneuver-horizon-and-hmi.md)）
   - `jp=next`: 発話した時点（ASR 開始時刻）の **次の難所 1 件** に固定する。締切を過ぎていれば理由を表示して終わる（2 件目には移らない）。該当がなければ全体を `UNKNOWN_REF` で却下する。
   - `jp=last`: 通過してから 180 s 以内かつ 1.5 km 以内の直近の難所。経路は変えずに `profile_proposals` へ入れる。該当がなくてもエラーにはせず、提案を作らないだけで、他のキーは確認へ進む。
5. **全部適用するか、何も適用しないか**: 静的検証に 1 つでも違反があれば全体を却下する。重みは正常でも迂回上限内の候補が 0 本なら、現行経路を維持して「この時間内では回避できる経路がありません」と出す。一部のキーだけ適用することはしない。
6. **却下理由は 7 種に固定する**: `SYNTAX | UNKNOWN_REF | STALE_REVISION | EXPIRED | FORBIDDEN_WHILE_DRIVING | NO_EFFECT | NO_PENDING_TARGET`（各コードの適用条件は [03 §4.2](../specs/03-routing-and-two-layer-control.md)）。
7. **確定入力を SLM から切り離す**
   - 経路を確定させる入口は **「Rust 辞書照合で一致した `dict_norm`」と `user_tap` の 2 つだけ**（どちらも `dc_tick` のイベント、[ADR-0008](0008-dc-tick-single-event-entry.md)）。定型語（はい / いいえ / やめて / キャンセル等）は SLM を通さず Rust の辞書照合で処理する（[05 §3.2](../specs/05-inference-tier-and-audio.md)）。
   - SLM が `YES` を出しても確定しない。再確認を出すだけにする。
   - 承認する時点で `target_id`（提案 ID・経路版・対象難所・期限）を検証し直す。発話している間に提案が差し替わった場合、前の提案への承認を新しい提案に引き継がない。
8. **SLM には文面を作らせない**: `WHY` で SLM が出すのは分類トークンだけで、説明文は `RouteComparison` の確定値から Kotlin のテンプレートで作る（[05 §4.1](../specs/05-inference-tier-and-audio.md)）。回避チップの `AvoidOffer.summary` も同じく、SLM は関わらずテンプレートで作る（[04 §2](../specs/04-maneuver-horizon-and-hmi.md)）。
9. **L3 の出力を L2 に入れる経路は `profile_proposals` だけ**: 到着後レビュー S12 で明示承認されたときだけ L2 本表に反映する（`source='review_approved'` と `proposal_id` を記録）。`jp=last` は 1 回の依頼で S12 に載り、T3（自動提案）はこれまでどおり 2 回以上の回避実績が条件。`profile_weights.source` から `'slm_preset'` を外す（DDL は [03 §5](../specs/03-routing-and-two-layer-control.md)）。

## 検討した代替案
- **v1.1 の「固定命令コード（例 `right_crossing=high`）を Rust で展開」だけを書いた短縮表現**: 文法、コードの種類、キーの順序、参照先の決め方、却下理由が書かれていなかった（コンテキスト参照）。本 ADR はこれを 7 種のコードと GBNF に置き換える。置き換える理由は、正本に文章として書かれていない。
- **SLM の `YES` /「はい」の解釈結果で経路を確定する案（v1.1 は明示同意を「はい」等とだけ書いていた）**: 正本は「SLM の YES では確定しない（再確認のみ）」と決め、確定入口を辞書照合の `dict_norm` と `user_tap` に限った。あわせて承認時に `target_id` を検証し直し、Gate 4 / 7 の共通テスト（発話中の提案差し替え、承認直前の逸脱、表示後の締切超過）で経路変更 0・古い案内発話 0 を確かめる。SLM の `YES` を確定入口から外す理由そのものは、正本に文章として書かれていない。
- **Kotlin が命令コードを JSON に組み直してから Rust に渡す案**: 正本は「Kotlin で JSON に組み直さない」とだけ決めており、理由は書いていない。
- **`profile_weights.source` に `'slm_preset'` を許す案（v1.1 の DDL）**: 正本は `'slm_preset'` を外し、`profile_proposals` を通して S12 で承認したときだけ反映すると決めた。理由は正本に書かれていない。

## 影響とトレードオフ
- **利点**: SLM の出力が何であっても、経路を変えるには Rust の静的検証・迂回上限の判定と、決定論の確定入口（`dict_norm` / `user_tap`）の両方を通る。命令コードの形が閉じているので、Gate 4 の期待コードを録音前に固定して採点できる。
- **利点**: 部分適用をしないため、走行中に「一部だけ反映された経路」は生まれない。
- **制約**: 7 種のコードで表せない要望は、`NOOP` か聞き返し、または却下になる。走行中に表せる内容は狭くなる。
- **制約**: SLM が `YES` と解釈しても再確認がもう 1 回入るので、音声だけで確定するまでの手数が増える。辞書照合に一致する定型語ならこの段は入らない。
- **制約**: `jp=next` / `jp=last` の解決は `ManeuverHorizon.refs` と発話時刻（`utterance_mono_ms`・`ref_frame_seq`）が正しいことに依存する。

## 再評価条件
- **Gate 4 の採点**（120 発話。期待コードは録音前に 2 人で付けて固定）: 明確 40 件（分母）で正答 ≥ 90%。曖昧 30 件は正しいコード / `NOOP` / 聞き返しなら合格で、それ以外の操作を実行したら契約外実行。否定 20 件・契約外 20 件は期待コードか `NOOP` 以外なら失敗。雑音 10 件（明確な命令＋車内雑音）は曖昧と同じ安全判定。未達の場合、モデル側の手当ては [ADR-0004](0004-isolated-inference-and-hardware-tiers.md) の再評価条件（語彙・候補 ID の絞り込み → 停車中限定の 1.5B 別評価等）に従う。1 行命令コードの形式そのもの（コード種別・GBNF）を見直すかどうかの対応は [07 §2.6](../specs/07-parameters-and-poc-gates.md) の表のとおり。
- **Gate 4 / 7 の共通テスト**（「発話中に提案差し替え」「承認直前に逸脱」「表示後に締切超過」）で経路変更 0・古い案内発話 0 を満たせない場合の対応は [07 §2.6](../specs/07-parameters-and-poc-gates.md) の表のとおり。
