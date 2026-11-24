# 用語集 (Glossary)

本プロジェクトの設計仕様書および ADR で使用する固有概念・統一用語の定義です。

| 用語 | 定義 |
|---|---|
| **二層制御 (L1 / L2 / L3)** | 経路選定・法規・安全は **L1（標準ロジック比較）** と **L2（承認済み個人設定の決定論適用）** が 100% 決定論で担い、**L3（ローカル SLM / BYOK LLM）** は個人制約の型付き差分提案のみを行う設計原則。 |
| **Intent Canvas** | 自然文や音声で入力された漠然とした意図を、編集可能な「意図チップ（必須/希望）」・地図上の比較・トレードオフ表示へ変換する出発前/停車中 UI（[01-product-and-ux](01-product-and-ux.md)）。 |
| **Intent Contract** | 出発前 Canvas でユーザーが確定・許可した「走行中に AI が介入・提案してよい範囲」（許容迂回・歴史解説の有無・許可命令集合）。 |
| **対向横断右折（右折侵入）** | 日本の左側通行において、対向車線を横切って道路へ進入・離脱する右折。無信号右折は最も運転負担が高いため回避対象の中心とする。 |
| **根拠 3 値 (`Evidence<T>`)** | 道路・交差点属性の信頼度区分。`confirmed`（実測・確認済み一次データ）、`inferred`（形状・道路種別等からの推定）、`unknown`（タグ欠損・版不一致・矛盾による未確認）。 |
| **`ManeuverHorizon` (`ManeuverLadder`)** | 走行中の Android 3 手先表示と P0〜P3 音声スケジューラが単一ソースとして参照する、Rust コア生成の最大 3 操作先の不変スナップショット（[04-maneuver-horizon-and-hmi](04-maneuver-horizon-and-hmi.md)）。 |
| **Maneuver Horizon（空白時間）** | 次の P1 ナビ案内締切（または複雑交差点窓）までに残された安全な時間的余白。P2/P3 音声発話の可否判定基準。 |
| **再探索回廊 (Corridor)** | 圏外でも自前リルートが破綻しないよう、出発前に経路本体と合わせて端末へ事前導入しておく「経路周囲 1 セル分」の Valhalla タイル・PMTiles 領域。 |
| **走行中ソフトモード** | 地図のパン・ズームを強制ロックしない代わりに、文字入力フォーカスや複雑な操作要素を折りたたみ、視認性の高い 3 手先表示と大ボタンへ切り替える適応 UI。 |
| **`:inference` 別プロセス隔離** | GPU/NPU ドライバやネイティブ推論のクラッシュ・ハングからナビ本体を守るため、SLM 推論を Android 非公開バインドサービス (`android:process=":inference"`) へ分離する構成。 |
| **Tier 0 / 1 / 2** | 実機スペック（RAM・GPU/NPU 安定性）と ADPF 熱状態に応じた推論切替段階。Tier 0＝決定論のみ、Tier 1＝CPU NEON 0.5B、Tier 2＝検証済み GPU/NPU 加速（[05-inference-tier-and-audio](05-inference-tier-and-audio.md)）。 |
| **BYOK (Bring Your Own Key)** | クラウド LLM 利用時（停車中の高度な旅程計画等）に、運営サーバを経由せずユーザー自身の API キーを端末内 Keystore へ保存して直接呼び出す方式。 |
| **ABR** | デジタル庁が整備する「アドレス・ベース・レジストリ」。日本の町字・住所検索の一次データソースとして使用（代表点は車両入口として流用しない）。 |
| **PMTiles** | 単一ファイルでクラウドストレージ（Cloudflare R2 等）の HTTP Range リクエストおよび端末内ローカル読込に対応したベクタータイル形式。 |
| **`DetourPolicy`** | L1/L2/L3 共通の迂回上限方針。採否は候補 ETA の増加秒数で判定し、`time_cap`（短距離は `short_cap_s`、それ以外は `long_cap_s` と `base_eta × long_cap_ratio` の小さい方）と `base_eta × (ratio_cap − 1)` の小さい方を上限とする。`dc_rank` と `dc_avoid_offer_eval` が同じ関数を使う（式は [03 §3.2](03-routing-and-two-layer-control.md)、数値は [07 §1](07-parameters-and-poc-gates.md)）。 |
| **`tight_factor`** | `DetourPolicy` の係数。命令コードで `dt=T` が指定されたとき迂回上限に掛ける（[03 §3.2](03-routing-and-two-layer-control.md)、数値は [07 §1](07-parameters-and-poc-gates.md)）。 |
| **`profile_proposals`** | L3 出力（`jp=last` や自動提案）を一時的に置く表 `(id, origin, payload_json, created_at, status)`。S12 で明示承認されたときだけ `profile_weights` へ反映する（`source='review_approved'` + `proposal_id`）（[03 §5](03-routing-and-two-layer-control.md)）。 |
| **`match_status`** | ハード制約の地図照合状態（`matched` / `needs_rematch` / `orphaned`）。`orphaned` のハード制約は黙って消さず S12 で通知する（[03 §5](03-routing-and-two-layer-control.md)）。 |
| **`refs`（`ManeuverHorizon.refs`）** | `{ next?: JunctionRef, last?: PassedRef }`（`JunctionRef{ref, maneuver_id, kind, deadline_mono_ms}` / `PassedRef{ref, maneuver_id, passed_at_mono_ms, passed_along_cm}`）。`frame_seq` と同じ版で持つ、`jp=next` / `jp=last` が指す難所の参照表の正本（[04 §2](04-maneuver-horizon-and-hmi.md)）。 |
| **1 行命令コード** | 走行中 L3 の出力形式。`OVR` / `CXL` / `WHY` / `BMK` / `YES` / `NO` / `NOOP` のいずれか 1 行を GBNF で強制し、解析は Rust（`dc_validate_override`）が行う。GBNF の正本は `schema/slm/override.gbnf`（[03 §4.1](03-routing-and-two-layer-control.md)、[ADR-0007](../adr/0007-l3-one-line-command-and-confirm-separation.md)）。 |
| **`user_tap`** | 利用者のタップを表す `dc_tick` イベント `{target_id, action: confirm \| avoid \| undo}`。`target_id` は回避チップの `offer_id`、または `override_id` / `confirm_screen_id`。辞書照合で一致した `dict_norm` と並ぶ、経路を確定できる 2 つの入口の一つ（[03 §4.2](03-routing-and-two-layer-control.md)、[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。 |
| **`dict_norm`** | `asr_result` に付く、Rust 辞書照合で一致した定型語の正規形。経路の確定入口の一つで、SLM の `YES` では確定しない（再確認のみ）（[03 §4.2](03-routing-and-two-layer-control.md)）。 |
| **`text_class`** | `asr_result` の発話分類（`dict` \| `natural`）。ログには発話の生文字列を残さず、`text_class` と辞書 ID のみ記録する（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。 |
| **却下理由（`NO_PENDING_TARGET` 等）** | L3 命令の検証で返る理由コード：`SYNTAX` / `UNKNOWN_REF` / `STALE_REVISION` / `EXPIRED` / `FORBIDDEN_WHILE_DRIVING` / `NO_EFFECT` / `NO_PENDING_TARGET`。静的検証違反は全却下で、部分適用はしない。各コードの適用条件は [03 §4.2](03-routing-and-two-layer-control.md)。 |
| **`search_finished`** | 探索の終端イベント `{req_id, route_key, graph_version, outcome: ok \| cancelled \| error, comparison_ref?, engine_ms}`（timeout は無い。監視上限の超過は Rust が検出して `engine_stuck`）。1 探索につき必ず 1 回届き、これが来るまで次の `start` は出さない（[03 §3.1](03-routing-and-two-layer-control.md)）。 |
| **`planner_cmds`** | `dc_tick` の出力で、Rust が出し Kotlin が実行する探索指示（`start{req_id, priority, route_key, graph_version}` \| `cancel{req_id}`）（[03 §3.1](03-routing-and-two-layer-control.md)、[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。 |
| **`discarded_late`** | cancel 後に `outcome=ok` が返った探索の記録。結果は使わず探索枠だけ解放する（[03 §3.1](03-routing-and-two-layer-control.md)）。 |
| **`discarded_after_stuck`** | `engine_stuck` の後に `outcome=ok` で戻った探索の記録。結果は捨て、探索枠だけ解放する。逸脱再探索がまだ必要なら直後に新しい `start` を出す（[03 §3.1](03-routing-and-two-layer-control.md)）。 |
| **`engine_stuck` / `engine_reset`** | `engine_stuck`：探索がネイティブ呼び出し開始からの監視上限（5 s / 10 s、暫定）を超えたことを Rust が `now_mono_ms` から検出した状態（Kotlin は通知しない）。「経路を確認中」を表示し、探索枠は占有したまま、ネイティブ呼び出しが実際に戻った時点で `search_finished` を 1 回返す（`ok` でも結果は捨て `discarded_after_stuck`）。`engine_reset`：探索エンジンのリセットを伝える `dc_tick` イベント（可否は Gate 0 で確認）（[03 §3.1](03-routing-and-two-layer-control.md)、[07 §2](07-parameters-and-poc-gates.md)）。 |
| **`dc_tick`** | Rust コアの唯一のイベント入口。`{ now_mono_ms, fix?, events }` を受け、`{ horizon?, voice_cmds, ui_cmds, planner_cmds, next_wake_mono_ms, log }` を返す。Rust は状態を持つが IO・スレッド・時計を持たない（型定義全文の正本は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。 |
| **`next_wake_mono_ms`** | `dc_tick` の出力。新しい入力がなくても出力が変わり得る最も早い単調時刻（ミリ秒、探索の監視期限を含む）で、それまでに `dc_tick` を呼ぶ。待つものがなければ null（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。 |
| **属性 sidecar** | Valhalla タイル単位の固定長バイナリ属性ファイル。mmap し、タイル内有向辺インデックスで引く。manifest で `graph_version`・SHA-256 と組にし、C++ は SQLite を使わない。元タグ欠損は `unknown`、sidecar の欠落・破損はデータ不完全として扱う。バイナリ仕様の正本は `pipeline/packs/sidecar-format.md`（[02 §1](02-architecture-and-stack.md)、[03 §2](03-routing-and-two-layer-control.md)）。 |
