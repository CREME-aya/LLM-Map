# 03. 二層制御と経路探索・採点契約

> **対象フェーズ**: Phase 1（L1/L2 確定基盤＋L3 限定介入）〜 Phase 2（Valhalla `AutoCost` 拡張）
> **関連 ADR**: [ADR-0001](../adr/0001-two-layer-deterministic-routing-first.md) / [ADR-0007](../adr/0007-l3-one-line-command-and-confirm-separation.md)（走行中 L3 1 行命令コードと確定入力の分離） / [ADR-0008](../adr/0008-dc-tick-single-event-entry.md)（`dc_tick` を唯一のイベント入口に）

---

## 1. 二層制御の基本原則（ロジック比較ファースト）

本アプリでは、**日常の経路選定・運転しやすさ評価・法規遵守・案内を 100% 決定論的ロジックで制御** し、**LLM / SLM は「個人の好みや自然な言葉の要望を、検証済みの制約・重み・経由候補の差分へ変換する必要がある場合のみ」介入** させます。

| 層 | 名称 | 実行方式 | 対象と発火条件 | 画面表示・ユーザー制御 |
|---|---|---|---|---|
| **L1** | **標準ロジック比較**（既定） | 100% 決定論（Valhalla 候補探索 ＋ Rust 3 軸採点） | すべての通常検索、定型チップ切替、逸脱再探索、通勤ドライブ | 既定の比較表示（SLM 不使用・追加発熱ゼロ） |
| **L2** | **個人設定（決定論適用）** | 100% 決定論（端末内 SQLite プロファイルの構造化適用） | 手動登録した苦手交差点メモ、承認済みの重み、保存済みプリセット | `あなた向け・N件適用`＋「なぜ？」で出どころを 1 行表示 |
| **L3** | **AI 個人経路最適化**（必要時のみ） | ローカル SLM（0.5B Q4, GBNF）または停車中 BYOK LLM → **Safety Gate 経由** | 下記 4 つのエスカレーション条件（T1〜T4）のいずれかに該当し、明示的に操作した時のみ | `AI調整` チップとして可視化（1 タップで解除可）。L1 最短経路は常に候補に残す |

> **鉄則**: AI（L3）は「計算ルール・法規」を書き換えず、「入力（制約・重み・許可候補参照）の差分」だけを提案します。座標・道路形状・誘導文の直接生成は禁止です。

### L3（AI 個人経路最適化）の発火トリガー（T1〜T4）
1. **T1（自然文の個人制約）**: 定型チップ外の要望（「子どもが乗っているから揺れの少ない大通り」「さっきのような無信号右折はもう避けたい」等）を入力・発話したとき。
2. **T2（既定候補の拒否）**: L1/L2 が提示した候補を連続で却下し「別の考え方でもう少し走りやすい道を」と求めたとき（停車中 / 出発前）。
3. **T3（過去回避からの学習提案）**: 案内された難所（無信号右折等）を 2 回以上意図的に別経路で回避した実績がある場合、**走行中ではなく到着後レビュー画面（S12）でのみ**「苦手な交差点として覚えますか？」と提案し、明示承認された時だけ L2 へ昇格。
4. **T4（長距離・複合最適化）**: 旅モード該当時、出発前に「途中の城跡を 1 か所かすめつつ右折少なめで」等の複合計画を要求したとき（BYOK クラウド LLM も選択可）。

### L3 出力の L2 昇格経路（`profile_proposals` 経由のみ）
L3 の出力は **L2 本表（`profile_weights` / `avoidance_nodes_edges`）へ直接書き込まず**、必ず提案表 `profile_proposals`（§5）へ入れます。**到着後レビュー（S12）で明示承認された時だけ** 本表へ反映し、そのとき `source='review_approved'` と元提案の `proposal_id` を記録します。

| 経路 | S12 に載る条件 | L2 反映 |
|---|---|---|
| **T3（自動提案）** | 従来どおり **2 回以上** の意図的回避実績 | S12 で明示承認された時のみ |
| **`jp=last`（走行中の明示依頼、§4.1）** | **1 回** の発話で S12 に載る（経路は変えない） | S12 で明示承認された時のみ |

---

## 2. コンポーネント統合境界とデータ契約

| 境界コンポーネント | 所有者 | 入力 → 出力 | 不変条件 |
|---|---|---|---|
| `RoutePlanner` | Android (Kotlin) | `TripPlanSnapshot` + 任意 `ValidatedPersonalOverride` → `RouteComparison` | 非同期実行・期限・地域パック DL を管理。`plan_request_id` とデータ版を固定し、遅延完了による上書きを禁止。 |
| `CandidatePlanner` | Rust (`drive-core`) | 確定目的地・車幅・通行制約・出発時刻・予算 → 順序付き `CandidateRequest[]` | 外部 IO なし。ラベルは `baseline` / `engine_alternate` / `costing_variant` / `through_anchor`。 |
| `RoutingEngine` | C++ (`valhalla`) | `CandidateRequest` → `EngineCandidate` or `EngineError` | アクセス制限・右左折規制・車幅規制・タイル完全性の信頼境界。タイル欠落は不完全エラーとして分離。 |
| `RouteEnricher` | C++ + Rust | Trip protobuf 有向辺・位置参照 ＋ 同版属性 sidecar → `CandidateFacts` | C++ が同一呼出しで graph edge ID と方向を解決し、Rust が区間距離・難所数・根拠 3 値を純粋集計。shape からの再推定は禁止。**属性 sidecar** はタイル単位の固定長バイナリを mmap し、タイル内有向辺インデックスで引く。manifest で `graph_version`・SHA-256 と組にする。C++ は SQLite を使わない（[ADR-0002](../adr/0002-narrow-pure-rust-core.md) の二重リンク回避）。元タグ欠損 = `unknown`、sidecar 欠落/破損 = データ不完全（不完全エラー扱い）。バイト配置は `pipeline/packs/sidecar-format.md` が正本（詳細は [02 §1](02-architecture-and-stack.md)）。 |
| `CandidateRanker` | Rust (`drive-core`) | `CandidateFacts[]` + `policy_version` + 承認済み選好 → `RouteComparison` | 合法・完全・同版の候補のみ採点。有効候補が 1 本だけでも理由付きで返す。 |
| `PersonalIntentAdapter` | Android (`:inference`) / BYOK | 明示要求 + 最小限の選択コンテキスト → 未検証 `PersonalRoutingOverride` 候補 | GBNF 制約の有無を問わず、出力はすべて未信頼入力として扱う。 |
| `OverrideValidator` | Rust (`drive-core`) | 未検証候補 + `IntentContract` + 許可済み候補 ID 表 → `ValidatedPersonalOverride` | 外部座標・URL・関数呼出し・未知/重複キー・範囲外・生 Valhalla パラメータをすべて拒否。 |

### `TripPlanSnapshot` の主要保持項目
- `schema_version`, `trip_id`, `trip_revision`, `plan_request_id`, `graph_version`, `policy_version`
- 出発地、目的地 / 車両入口・歩行者出口の正規 ID（**住所代表点と車両入口点は別型**とし、代表点のみの場合は「入口未確認」扱い）
- 確定済み経由順、普通車寸法・通行制約（有料道路・フェリー・通行制限）、出発時刻

---

## 3. Phase 1 候補探索と 3 軸採点アルゴリズム

### 3.1 Valhalla の制約を踏まえた候補生成フロー
一次仕様の確認により、**現行 Valhalla では中間地点指定時に `alternates` が 0 本になる** ため、以下の決定論手順で候補を収集します。

1. **基準探索 (`baseline`)**: 既定 Auto costing で第 1 候補をまず生成・表示可能な状態にする（中間地点なし時のみ最大 2 本の `engine_alternate` も併せて要求）。
2. **追加探索の起動条件**: 候補の多様性が不足、主要な対向横断右折が残る、またはユーザーが比較を要求した場合、同版のまま以下を追加発行する。
   - **既存バリアント (`costing_variant`, 最大 2 件)**: 幹線道路選好、`service_penalty` による生活道路入口抑制など、現行 Auto パラメータの組合せ。
   - **決定論的経由点探索 (`through_anchor`, 最大 2 件)**: 難所の手前から迂回できる合法な通過辺／左側進入アプローチを同版グラフ上で検索し、U ターンを許す `via` ではなく **`through`** として渡す。難所への戻り遷移・ループ・隣接車線誤マッチを出力後に検証し破棄。
3. **予算とフェイルセーフ**: 合計上限 **5 要求・受理 7 候補・同時 1 探索**。比較結果を返す予算（比較予算）は初期値として **短距離 3 秒 / 長距離 8 秒**。期限超過時はその時点の完了候補で比較結果を返す（ネイティブ呼び出しの監視上限は下記「タイムアウトと `engine_stuck`」）。**走行中のルート逸脱再探索では既定 1 本を最優先** し、比較探索を割り込ませない（下記「探索キューと終端イベント」）。

#### 探索キューと終端イベント（`planner_cmds` / `search_finished`）
`RoutePlanner` は **単一の優先度キュー** を持ち、**同時実行は物理的に 1 本** です。キューの起動・取消は Rust が `dc_tick` の出力 `planner_cmds` で指示し、Kotlin が実行します（`dc_tick` の型定義全文は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md) が正本）。

| 優先順 | 探索種別 | 備考 |
|---:|---|---|
| 1 | **逸脱再探索**（`reroute`） | 走行中の最優先。下位の探索は cancel して枠を空ける |
| 2 | **明示計画**（`explicit_plan`） | — |
| 3 | **回避チップ**（`avoid_chip`） | 計算予算・起動条件は [04 §4.3](04-maneuver-horizon-and-hmi.md) |
| 4 | **比較探索**（`comparison`） | 走行中の逸脱再探索には割り込まない |

| 方向 | メッセージ | 内容 |
|---|---|---|
| Rust → Kotlin | `planner_cmds: start{req_id, priority, distance_class, route_key, graph_version}` | 探索開始。`priority` は上表の JSON 名、`distance_class` は下記 |
| Rust → Kotlin | `planner_cmds: cancel{req_id}` | 実行中探索の取消要求 |
| Rust → Kotlin | `planner_cmds: commit{req_id, route_key, undo_req_id?}` | 逸脱再探索の結果を確定する指示。Kotlin が `dc_route_commit` を呼ぶ（回避チップは **同意の前には出さない**。同意後に出す） |
| Rust → Kotlin | `planner_cmds: cancel_commit{req_id}` | 出した `commit` の取り消し（経路が入る前に「戻して」が来たとき）。Kotlin は経路本体を捨て、`dc_route_commit` を呼ばない。探索の取り消し `cancel{req_id}` とは別 |
| Rust → Kotlin | `planner_cmds: evaluate_offer{req_id, facts_ref, base_facts_ref}` | 回避チップの探索が `ok` で戻ったときの評価指示。Kotlin が `dc_avoid_offer_eval` を呼ぶ `base_facts_ref` は、回避チップの探索の `search_finished` に任意で付けて返す（Kotlin が代わりの経路と同時に作る） |
| Rust → Kotlin | `planner_cmds: rank{plan_request_id, facts_refs: [string], reason: all_done\|budget_expired}` | 計画の採点指示。Kotlin が `dc_rank` を呼んで比較結果を表示する |
| Kotlin → Rust | `search_finished{req_id, route_key, graph_version, outcome: ok\|cancelled\|error, facts_ref?, engine_ms}` | **唯一の終端イベント。1 探索につき必ず 1 回**。旧 `reroute_result` / `search_cancelled` は廃止 |

- **距離区分 `distance_class: short | long`（暫定）**: Planner が **探索の前に** 決める。探索前に使える ETA 見積もりが **30 分未満なら `short`、30 分以上または見積もりがなければ `long`**。同じ計画の候補探索の中では区分を固定する。比較予算（3 s / 8 s）は **計画開始から**、監視上限（5 s / 10 s）は **各ネイティブ呼び出しの開始から** 数え、どちらもこの区分で選ぶ。なお [§3.2](#32-重複除去と-3-軸採点ルール) の `DetourPolicy` の 30 分（`short_threshold_s`）は探索後の基準 ETA に適用する別の判定である。境界テストは 29:59 / 30:00 / 見積もりなしの 3 ケースと、監視上限超過後も `search_finished` まで探索枠が解放されないこと。

Rust 側の探索枠の状態機械:

```text
Idle ──start{req_id}──▶ Running{req_id}
Running ──search_finished{req_id}──▶ Idle
Running ──cancel{req_id}──▶ CancelRequested{req_id}
CancelRequested ──search_finished{req_id}──▶ Idle
  ※ outcome=ok で戻った場合は discarded_late として記録し、結果は使わず枠だけ解放
Running / CancelRequested ──監視期限超過（Rust が now_mono_ms で検出）──▶ engine_stuck{req_id}
engine_stuck ──search_finished{req_id}──▶ Idle
  ※ outcome=ok で戻っても結果は捨て、discarded_after_stuck として記録し枠だけ解放
  ※ 逸脱再探索がまだ必要なら、直後に新しい start を出す
不変条件: search_finished が来るまで次の start を出さない（同時 Running ≤ 1）
```

- **`facts_ref` と `rank`**: `search_finished.facts_ref` は、その探索 1 本で得た **採点前の `CandidateFacts` への参照**（`outcome=ok` のときだけ）。採点には計画の候補がすべて要るため、探索ごとには `dc_rank` を呼ばない（旧名 `comparison_ref` は廃止）。
  - Rust は、計画のすべての探索が終わったとき（`reason: all_done`）、または比較予算を超えたとき（`reason: budget_expired`）に `rank` を 1 回出す。`facts_refs` はその計画で `ok` になった探索の `facts_ref` を **要求した順** に並べたもの。空でも `rank` は出し、Kotlin の `dc_rank` は「有効候補 0 本」を理由付きで返す。
  - 予算超過時は待っている比較探索を取り消し、新しい `start` は出さない。実行中の探索は `search_finished` まで枠を占有したまま。
  - **計画の中の探索**: Rust（`Session::plan_candidates`）が計画ごとに `plan_request_id` と `distance_class` を固定し、候補を待ち行列に積む。基準経路は `explicit_plan`、追加候補は `comparison` の優先度で、**1 本ずつ** `start` を出す。`start` には任意の `candidate_id`（計画内の `CandidateRequest` を指す。計画の外の探索では付けない）を付ける。
  - **比較予算** = `min(距離区分の予算 3 s / 8 s, TripPlanSnapshot.budget.deadline_ms)`。`deadline_ms` は呼び出し側が予算を **短くする方向にだけ** 使え、無ければ距離区分の予算をそのまま使う。
  - **計画の候補を経路にする**：`rank` を出すと新しい探索は始めないが、**候補の `commit` はまだ受け付ける**（2 つの状態を分ける）。Rust は計画に `ok` で戻った探索の `{req_id, candidate_id, facts_ref}` と計画の `trip_revision` を持っておき、利用者が比較表から選んだ候補はその `req_id` で `dc_route_commit` に入れる（[02 §2.1](02-architecture-and-stack.md)）。候補を受け付けなくなるのは、次の計画が始まったとき、候補の 1 つが commit されたとき、走り出したとき（走行中ソフトモードの突入条件、暫定）。
  - **新しい計画が来たら**、古い計画は打ち切り、古い計画の `rank` は出さない（実行中の探索は `search_finished` まで枠を占有し、戻った結果は `discarded_late`）。
  - 計画の中の探索の `route_key.route_revision` は `"0"`（未確定）。版は経路を入れる `dc_route_commit` で Rust が付ける（[02 §2.1](02-architecture-and-stack.md)）。
  - **計画の外の探索（逸脱再探索・回避チップ）の `distance_class`**: 探索開始時点で、現在の経路に対する **有効な残り ETA が 30 分未満なら `short`、30 分以上・不明・失効なら `long`**。その探索の間は固定し、直前の計画の区分は引き継がない（逸脱で残り ETA が失効したら、直前の計画が `short` でも `long`）。計画の外でこの区分が選ぶのは **監視上限 5 s / 10 s だけ**（3 s / 8 s は出発前の計画の比較予算）。回避チップは引き続き **計算予算 1.5 s・分岐 20 s 前の締切** で提示を判断し、監視上限まで待って提示してよいという意味ではない。
  - **計画の外の探索結果の扱い**: 逸脱再探索が `ok` で戻り、`route_key` が一致し、`horizon.status` が `tracking` / `off_route` / `rerouting` のとき（[04 §3](04-maneuver-horizon-and-hmi.md)）だけ、Rust は `planner_cmds: commit{req_id, route_key}` を出す。`req_id` は Kotlin が保持する経路本体を引くキー、`route_key` は **その探索を始めた時点の現行経路の版**（新しく確定する経路の版ではない）。Kotlin は `req_id` に対応する経路本体（Valhalla から受け取って保持）を、`commit` の `route_key` を `expected_route_key` として `dc_route_commit(s, {expected_route_key, bundle})` に渡し（[02 §2.1](02-architecture-and-stack.md)。今の経路があるのに `null` なら `E_STALE`）、`dc_route_commit` はその `route_key` が今も現行経路と一致するかを確かめ直す。`commit` を出した後、`dc_route_commit` が呼ばれる前に経路が差し替わっていたら、経路は差し替えず `E_STALE` を返す（遅れて届いた経路は `horizon` にも `voice_cmds` にも出ない。Gate 7 で確認）。回避チップの探索が `ok` で戻っても、その時点では `commit` は出さず、`dc_avoid_offer_eval` を通して `AvoidOffer` を Horizon に載せ、利用者の同意を待つ（同意なしで経路を変えない）。同意（`user_tap{offer_id, avoid}` または辞書の「はい」）が締切前に届き、`offer_id` と `route_key` が一致したときだけ `commit` を出す。締切を過ぎたら `dismiss{reason: expired}` で取り下げる。同意後 5 秒以内の「戻して」（`undo` / `CXL`）の戻し方は下記。
  - **「戻して」の戻し方**: 同意して `commit` を出すとき、Rust が戻すときに使う ID（`undo-<回避の探索の req_id>`）を決めて `commit` に載せる（元の経路は計画の候補や最初の `dc_route_commit(null)` で入っていることが多く、Rust はその探索の `req_id` を持たないため）。Kotlin は回避した経路を入れるときに、それまでの経路本体をその ID で 5 秒間保持する。Rust は `undo` を受けたら `commit{req_id: undo-…, route_key: <今の版>}` を出す。Kotlin は保持していた経路本体の版を `"0"` に戻して渡し、Rust が今の版 + 1 を付ける（版は戻さない）。
    - 戻すときは、保留中の再探索結果を捨て、位置合わせを新しい経路でやり直す。
    - 戻すときに使う ID は、同意して出す `commit` の `undo_req_id`（`"undo-<req_id>"`）で Kotlin に知らせる。`CXL` で戻すと決めたときの `dc_validate_override` の結果は `Undone { req_id }`。
    - 5 秒を過ぎた `undo` / `CXL` は `NO_PENDING_TARGET`。`tracking` でないときは `NO_EFFECT`。戻せなかったときは `speak{template_id: undo_unavailable, priority: p2}`。
    - **同意して `commit` を出したが、回避した経路がまだ入っていない（`dc_route_commit` 前）ときの `undo`**：`NO_EFFECT` にせず、出した `commit` を取り消す。Rust は `planner_cmds: cancel_commit{req_id}` を出してその `req_id` を待つのをやめ、Kotlin は保持していた回避した経路の本体を捨てて `dc_route_commit` を呼ばない。それでも同じ `req_id` で呼ばれたら `E_STALE` で断る。経路は変わらないので「戻せた」と同じ扱い。呼び出しは actor 1 本が順に行うので、`dc_route_commit` と `undo` のどちらが先かは必ず 1 つに決まる（先なら通常の「戻して」、後なら取り消し。Gate 7 で両方の順番を確認）。
    - 戻した経路と回避した経路が分かれる地点（分岐）をすでに過ぎていたら戻さない（`NO_EFFECT`）。回避チップは分岐の 20 秒前（早い見込み）までにしか同意できないので、5 秒以内なら通常は分岐の手前にいるが、ETA の見込み外れに備えて Rust が位置で確かめる。分岐点は **回避した経路を入れる `dc_route_commit` のときに 1 回だけ** 求めて持っておく（「戻して」のときに計算し直さない。その時点なら Rust は今の経路と新しい経路の本体を両方持っており、`dc_avoid_offer_eval` の入力（`CandidateFacts`）に edge 列を足さずに済む。「戻して」は commit の後にしか起きないので、提案を作るときに求めても結果は同じ）。探索開始時の位置は `AvoidOffer` に記録しておく。求め方：
      1. 回避した経路の最初の edge（探索開始時の位置で、途中から始まる）を、元の経路の edge 列のうち **探索開始時の位置より先** の部分から探す（「今の位置」ではない。探索中も車は進んでいるため）。
      2. 見つかった場所から、2 本の列を向き付き edge ID で前に向かって比べ、最初に違うところを分岐点とし、**回避した後の経路の** `along_route_cm` で持つ。
      3. 最初の edge が見つからない、見つかった場所が複数あって 1 つに決まらない（ループする経路など）ときは、分岐点を「不明」とし、その経路変更は「戻して」の対象にしない（`NO_EFFECT`）。
    - 今の位置が **分岐点ちょうど、またはその先** なら戻さない（`NO_EFFECT`）。位置が `uncertain` のときは過ぎたか分からないので、安全側に倒して戻さない（`NO_EFFECT`）。戻せなかったときは、利用者に短い定型文（テンプレート）で伝える。「経路を確認中」の表示は `horizon.status` だけで出す。
  - `rank` を出した時点で Rust はその計画を **終了** とし、以後その計画の `start` を出さない。完了を知らせるイベントは無い。`rank` 後に同じ計画の `search_finished` が戻ったら `discarded_late` として記録し、枠の解放にだけ使う。

- **結果を採用する条件**: `outcome=ok` **かつ** `route_key` が現行と一致 **かつ** 走行状態が `tracking` のときのみ。それ以外は破棄（表示・発話に使わない）。
- **タイムアウトと `engine_stuck`**: ネイティブ呼び出し開始からの **監視上限（短距離 5 s / 長距離 10 s、暫定。値は [07 §1](07-parameters-and-poc-gates.md)）** の超過は **Rust が検出** する（`now_mono_ms` と `start` を出した時刻から判定）。Kotlin は timeout を通知せず、`next_wake_mono_ms`（監視期限を含む）に合わせて `dc_tick` を呼ぶだけ。超過したら Rust は `engine_stuck` に入る。画面の扱いは探索の種類で分ける：逸脱再探索中なら `horizon.status` は `rerouting` のまま（もともと「経路を確認中」を表示している）。回避チップの探索中なら今の経路は有効で案内も続いているので `status` を変えず、`engine_stuck` をログに残すだけにして、探索枠が空くまで新しい回避チップを出さない（`status` を変えると [04 §3](04-maneuver-horizon-and-hmi.md) の抑制で P2/P3 や 3 手先の提案まで止まるため）。そのため `search_finished.outcome` に `timeout` は無い。**探索枠は占有したまま** とし、ネイティブ呼び出しが **実際に戻った時点で `search_finished` を 1 回** 返す。探索枠を解放するのはこの `search_finished` のときで、`ok` で戻っても結果は捨て `discarded_after_stuck` として記録する（cancel 後の `ok` は従来どおり `discarded_late`）。逸脱再探索がまだ必要なら直後に新しい `start` を出す。新しい Actor を作れただけでは再開しない。エンジン再初期化は `engine_reset` イベントで Rust に通知する（可否は 未確認（Gate 0 で確認））。
- **Valhalla 割り込み**: 根拠 — 固定版ソースで割り込みコールバック対応を確認済み（sol6.2さん）。valhalla-mobile 0.6.2 に (1) `route` への割り込みコールバック、(2) lock-free アトミックフラグによる `cancel(req_id)` のパッチを当てる。中断例外は **C++ ラッパー内で必ず捕捉** し `outcome=cancelled` の終端に変換する（例外を JNI 越しに投げない）。cancel 後に同じ Actor で次探索が成功するかは 未確認（Gate 0 で確認）。
- **先読みの既定**: 停止遅延を実測するまで、**走行中の回避チップ先読みは無効** のまま PoC を進める。
- 共通テストデータ（逸脱による cancel → 再探索の順序）は [07 §2](07-parameters-and-poc-gates.md) の Gate 7 を参照。

### 3.2 重複除去と 3 軸採点ルール
- **指紋化による重複除去**: `graph_version` ＋ legs ＋ 方向付き edge 列 ＋ 部分辺始終比で指紋化し、実質同一の難所・進入方向を持つ重複候補を比較一覧から整理する。
- **比較基準と迂回上限**: 受理された合法・完全候補の中で最小 ETA の経路を基準（`baseline_id`、その ETA を `base_eta`）とし、候補の採否は **増加秒数** で判定する（比較基準更新時は再判定）。

  ```text
  cand_eta - base_eta <= min(time_cap, base_eta * (ratio_cap - 1)) * (dt=T ? tight_factor : 1)

  time_cap = base_eta <  short_threshold_s ? short_cap_s
           : min(long_cap_s, base_eta * long_cap_ratio)
  ```

  - パラメータ（`DetourPolicy` の `short_threshold_s` / `short_cap_s` / `long_cap_s` / `long_cap_ratio` / `ratio_cap` / `tight_factor`）の数値は [07 §1](07-parameters-and-poc-gates.md) が正本（すべて暫定初期値）。
  - **L1/L2/L3 共通** の式。`dc_rank`（候補採点）と `dc_avoid_offer_eval`（回避チップ評価）は **同じ関数** を使う。
  - `dt=T` は §4.1 の L3 命令で迂回を厳しくした場合（`tight_factor` を乗じる）。
  - **走行中の回避チップ** では `base_eta` = 現行経路の **残り ETA**。
  - ユーザー値 `profile_weights.max_detour_ratio`（§5）は既定 NULL（= 方針値）。設定時は **厳しくする方向にのみ有効** で、1.35 で頭打ち（DDL の CHECK で 1.0〜1.35）。採否式では増加秒数に換算して使う。
- **表示の 3 軸（固定）**:
  1. **所要時間差**: 基準経路との差分（快適性ペナルティは比較順位のスコア計算のみに使い、ETA 表示自体は改変しない）。
  2. **対向横断右折数**: 「信号なし確認」「信号あり・保護未確認」「信号未確認」を分けてカウント（＋右折入口件数）。
  3. **狭路距離**: 「確認済み狭路」「推定狭路」を分け、「幅未確認の生活道路距離」を併記。

#### 根拠の 3 値（`Evidence<T>`）と安全側スコア初期値
- **`confirmed`**: 方向付き実測・現地記録または信頼済み一次タグで確定。
- **`inferred`**: 車線数・道路種別・形状等による傍証（`lanes` から物理幅メートル値を捏造しない。無タグ信号は「保護不明」扱い、信号＋右折レーンだけで保護現示を断定しない）。
- **`unknown`**: タグ欠損・版不一致・古い記録の矛盾（幅未確認の生活道路を、既知の右折回避のために無条件で優遇しない）。

| 評価項目（左側通行・普通車） | 暫定ペナルティ（秒相当・PoC 校正前提） |
|---|---:|
| 対向横断右折（信号なし／信号未確認） | +45 秒 / 回 |
| 対向横断右折（信号あり・保護未確認） | +25 秒 / 回 |
| 対向横断右折（保護現示確認） | +10 秒 / 回 |
| 対向横断なしを確認した右折 | +0 秒 / 回 |
| 目的地への対向横断入口（確認済み） | +90 秒（置換） |
| 目的地入口の方向不明 | +45 秒 |
| 狭い対面道路（暫定基準 4.0 m 未満） | +30 秒 / 100 m |
| 生活道路で幅未確認 | +15 秒 / 100 m |
| ランプから本線への合流 | +15 秒 / 回 |

#### `dc_rank` の採点式（暫定。すべて整数で計算し、浮動小数点を使わない）
1. **候補の絞り込み**: 合法・完全・同版の候補だけを使い、上の迂回上限の式を満たさない候補は比較表から外す。基準経路（`baseline_id`）は必ず残す（§1「L1 最短経路は常に候補に残す」）。
2. **総合スコア（ミリ秒相当）**: `score_ms = eta_ms + Σ penalty_ms × weight_permille / 1000`（切り捨て）。軸ごとの正規化はしない（ペナルティがもともと「秒相当」で ETA と同じ単位のため）。
   - 回数で数える項目（右折・合流など）：`penalty_ms × 件数`。距離で数える項目（狭路・幅未確認の生活道路）：`penalty_ms_per_100m × length_cm / 10000`（切り捨て）。
   - **目的地への対向横断入口（+90 秒、置換）** は、その到着操作の右折ペナルティを **置き換える**（足し合わせない）。入口の方向が不明なら +45 秒。
3. **重み `weight_permille`（千分率の整数）**: 軸（右折 / 狭路 / 合流）ごとに `L2 の倍率 × L3 の倍率 / 1000`。
   - L2：`profile_weights.*_penalty_scale` を千分率に換算した値（既定 1000）。
   - L3：§4.1 の `low / normal / high` を **500 / 1000 / 2000**（暫定。0 倍・負数は禁止のまま）。L3 の指定がなければ 1000。
4. **並び順**: `score_ms` の小さい順。同点なら **ETA → 未知区間距離 → 候補指紋（文字列として辞書順）** の順で比べる（決定論）。
5. **表示用の数**（3 軸）は、ペナルティや重みを掛けない **生の件数・距離** を `CandidateFacts.counts` / `narrow_m` / `unknown_width_residential_m` から出す（ETA 表示は改変しない、§3.2 の 1 のとおり）。
6. ペナルティの値は上の表と [07 §1](07-parameters-and-poc-gates.md)（正本）。

*(タイブレーク順: 総合スコア → ETA → 未知区間距離 → 候補指紋。Phase 2 では `RoutingEngine` の型付き `Capability` と `CostPolicyVersion` を介して、同じ整数特徴を C++ `AutoCost` の forward/reverse およびショートカット集約コストへ直接適用します。)*

---

## 4. `PersonalRoutingOverride`（L3 契約）と Safety Gate

### 4.1 型付き命令契約
- **停車中・出発前 JSON 契約例**:
```json
{
  "schema_version": 1,
  "action": "suggest_route_preference",
  "weights": {
    "crossing_right": "high",
    "narrow_road": "high",
    "merge": "normal"
  },
  "junction_preferences": [
    { "ref": "j1", "mode": "avoid_if_practical" }
  ],
  "through_ref": "a1",
  "detour": "standard"
}
```
- **列挙制限**: `weights` 各項は `low / normal / high` の閉じた列挙（政策側で正の整数倍率へ写像、0 倍・負数禁止）。`junction_preferences` は最大 3 個、`through_ref` は停車中のみ最大 1 個とし、事前に候補表で割り当てた参照 ID 以外は拒否します。
- **走行中 0.5B SLM 向け 1 行命令コード**（[ADR-0007](../adr/0007-l3-one-line-command-and-confirm-separation.md)）: 32 tokens 上限内に収めるため、走行中は GBNF により **1 行の命令コード** のみを生成させ、Rust 側で上記同一型へ展開します。

| コード | 意味 |
|---|---|
| `OVR` | 選好の上書き（下記キーを 1 個以上。空 `OVR` は文法で不可） |
| `CXL` | 取消。確認待ちの提案を取り消す、または確定後 **5 秒以内** の変更を取り消す（[04 §4.3](04-maneuver-horizon-and-hmi.md) の「戻して」） |
| `WHY` | 理由の問い合わせ。SLM は分類トークンのみ出し、文面は `RouteComparison` の確定値からテンプレート生成（[05 §4.1](05-inference-tier-and-audio.md)） |
| `BMK` | ブックマーク。再生中または直前の歴史・POI を「あとで見る」へ保存 |
| `YES` / `NO` | 応答。**`YES` で経路は確定しない**（再確認のみ。確定入口は §4.2） |
| `NOOP` | 何もしない（曖昧・契約外の発話の受け皿） |

| `OVR` のキー | 値 | 意味 |
|---|---|---|
| `rc=` | `L` / `N` / `H` | 対向横断右折の重み（`weights.crossing_right` の `low / normal / high`） |
| `nr=` | `L` / `N` / `H` | 狭路の重み（`weights.narrow_road`） |
| `mg=` | `L` / `N` / `H` | 合流の重み（`weights.merge`） |
| `dt=` | `T` / `S` のみ | 迂回上限。`T` は §3.2 の式で `tight_factor` を乗じる、`S` は方針値のまま |
| `jp=` | `next` / `last` | 対象難所の指定（下記） |

- **`jp=next`**: 発話時点（ASR 開始時刻）の **次の難所 1 件に固定**（参照は `ManeuverHorizon.refs.next`、[04 §2](04-maneuver-horizon-and-hmi.md)）。その難所が締切済みなら `EXPIRED` で理由を表示して終了し、2 つ目の難所には飛ばない。該当難所がなければ全却下 `UNKNOWN_REF`。
- **`jp=last`**: 通過後 **180 s 以内かつ 1.5 km 以内** の直近難所（暫定初期値）。**経路は変えず** `profile_proposals`（§5）へ提案を入れ、S12 に 1 回で載る（§1）。該当なしはエラーにせず提案を作らないだけで、他のキーは確認へ進む。
- **解析は Rust**（`dc_validate_override`、§4.2）。Kotlin はコード行を JSON に組み直さず、そのまま Rust へ渡す。
- 評価用の期待命令コード表の正本は `schema/slm/expected_codes.json`。

**GBNF 全文**（正本ファイル `schema/slm/override.gbnf` の写し。差異があれば正本ファイルが優先。先頭分岐・固定順）:

```gbnf
root     ::= cmd "\n"
cmd      ::= override | "CXL" | "WHY" | "BMK" | "YES" | "NO" | "NOOP"
override ::= "OVR" ( rc nr? mg? dt? jp? | nr mg? dt? jp? | mg dt? jp? | dt jp? | jp )
rc ::= " rc=" level
nr ::= " nr=" level
mg ::= " mg=" level
dt ::= " dt=" ("T" | "S")
jp ::= " jp=" ("last" | "next")
level ::= "L" | "N" | "H"
```

### 4.2 `OverrideValidator`（Deterministic Safety Gate）
1. **構文・型検査**: 未知キー・重複キー・非整数値・過大サイズ・バージョン不一致を即拒否。
2. **値域・能力クランプ**: 迂回上限（§3.2 の増加秒数の式）や `IntentContract` が許可した範囲外（走行中の新規目的地変更・契約外変更等）を遮断。
3. **強い制約との分離**: 停車中にユーザーが明示登録した「この交差点を絶対通らない」等の強い制約は決定論 L2 の別型として保持し、モデルが柔らかな選好から勝手に昇格・緩和することを禁止。
4. **冪等適用と期限**: 走行中のプレビュー・変更提案は最大 **30 秒期限**、同一 `plan_request_id` / `trip_revision` に限定し、発火直前に現在進捗・P0/P1 切迫を再確認。タイムアウト・検証失敗・曖昧応答時は即座に L1/L2 の現行経路を維持します。

#### 判定の 2 段階（部分適用なし）
| 段階 | 条件 | 結果 |
|---|---|---|
| **① 静的検証** | 構文・参照・版・期限・走行中禁止などに 1 つでも違反 | **全却下**（`Rejected{code}`）。一部のキーだけ適用することはしない |
| **② 再採点** | 重みは正常だが、§3.2 の迂回上限内の候補が **0 本** | **現行経路を維持** し「この時間内では回避できる経路がありません」と表示 |

#### 却下理由（7 種）
| 却下理由 | 適用条件 |
|---|---|
| `SYNTAX` | 文法・解析の失敗 |
| `UNKNOWN_REF` | 参照先が見つからない（例: `jp=next` で該当難所なし） |
| `STALE_REVISION` | `route_key` / `trip_revision` / `ref_frame_seq` の不一致 |
| `EXPIRED` | 期限切れ、および `jp=next` の対象難所が締切済み |
| `FORBIDDEN_WHILE_DRIVING` | 走行中の `through_ref` 指定・目的地変更 |
| `NO_EFFECT` | 現行と同じ重み（適用しても変化なし） |
| `NO_PENDING_TARGET` | `YES` / `NO` / `CXL` の対象（確認待ちの提案など）がない |

#### 経路を確定する入口（2 つだけ）
| 入口 | 内容 |
|---|---|
| **辞書照合の `dict_norm`** | Rust の辞書照合で一致した定型語（`asr_result{text_class: dict, dict_norm}`） |
| **`user_tap`** | 画面タップ（`user_tap{target_id, action}`。`target_id` は回避チップの `offer_id`、または `override_id` / `confirm_screen_id`） |

- **SLM の `YES` では確定しない**（再確認を促すのみ）。
- 承認時に `target_id`（提案 ID・経路版・対象難所・期限）を **再検証** する。発話中に提案が差し替わった場合、**承認を新しい提案へ引き継がない**。
- イベントの型は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md) の `dc_tick` v2 が正本。共通テスト（発話中の提案差し替え・承認直前の逸脱・表示後の締切超過で経路変更 0・古い案内発話 0）は [07 §2](07-parameters-and-poc-gates.md) の Gate 4 / 7。

#### `dc_validate_override` の入出力（Rust C ABI v1）
```text
dc_validate_override(h, { code_line, contract, utterance_id, utterance_mono_ms, ref_frame_seq, pending_offer_id? })
  → Applied  { override_id, override, review_proposal?, route_key, expires_at_mono_ms }
  | Undone   { req_id }      // CXL で回避チップの経路変更を戻すとき（§3.1）
  | Bookmarked { poi_id }    // BMK を受け付けたとき（05 §6.2）
  | Rejected { code }        // code は上記 7 種
```
- `code_line` は §4.1 の 1 行命令コードそのもの。`utterance_id` は **必須**（`dc_tick` の `asr_started` / `asr_result` と同じ ID。Gate 4 で遅延を突き合わせるため）。`utterance_mono_ms` / `expires_at_mono_ms` は単調時刻（§5「時刻の種類」）。ABI 全体は [02 §2.1](02-architecture-and-stack.md)。

---

## 5. 端末内 SQLite `Personal Driving Profile` スキーマ（L2 基盤）

個人プロファイルは **AndroidX `sqlite-bundled` 経由で端末内にのみ保存** し、クラウド送信しません。グラフ版更新時に古い Valhalla edge ID をそのまま使い回す誤マッチを防ぐため、交差点識別には大まかな位置と根拠特徴を併記し、版跨ぎ時は再照合を行います。

#### 時刻の種類
| 用途 | 時刻 | 永続化 |
|---|---|---|
| セッション内の締切（提案期限・`expires_at_mono_ms` 等） | **単調時刻**（`SystemClock.elapsedRealtime` 系、Kotlin が `now_mono_ms` で Rust へ渡す） | しない（非永続） |
| 180 日減衰の `expires_at` など DB に残す時刻 | **UNIX 時刻** | する |

```sql
-- 注記: CHECK 制約を使うため、この DDL は Room エンティティではなく
--       生 SQL / SQLDelight で管理する（02 §1）。
-- 実装規約: 外部キーを有効にするため、接続ごとに PRAGMA foreign_keys = ON; を実行する。

-- 1. コンテキスト別の運転しやすさ重みオフセット（承認済みL2）
CREATE TABLE profile_weights (
  context_key TEXT PRIMARY KEY,           -- 'default', 'night', 'rain', 'highway_long'
  right_turn_penalty_scale REAL NOT NULL DEFAULT 1.0,
  narrow_road_penalty_scale REAL NOT NULL DEFAULT 1.0,
  merge_penalty_scale REAL NOT NULL DEFAULT 1.0,
  max_detour_ratio REAL DEFAULT NULL      -- NULL = 方針値（03 §3.2 / 07 §1 DetourPolicy）。
    CHECK (max_detour_ratio IS NULL OR max_detour_ratio BETWEEN 1.0 AND 1.35),
                                          -- ユーザーが迂回を厳しくするための値（1.35 で頭打ち）。
                                          -- §3.2 の採否式では増加秒数に換算して使う
  source TEXT NOT NULL
    CHECK (source IN ('explicit_ui', 'review_approved', 'imported_preset')),
  proposal_id INTEGER REFERENCES profile_proposals(id),
                                          -- source='review_approved' のときの元提案
  updated_at INTEGER NOT NULL
);

-- 1b. L3 出力の提案表（S12 で明示承認された時だけ 1 / 2 の本表へ反映）
CREATE TABLE profile_proposals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  origin TEXT NOT NULL
    CHECK (origin IN ('l3_override', 'jp_last', 't3_auto')),
                                          -- L3 命令 / jp=last 明示依頼 / T3 自動提案
  payload_json TEXT NOT NULL,             -- 提案内容
  created_at INTEGER NOT NULL,            -- UNIX 時刻（永続）
  expires_at INTEGER NOT NULL,            -- UNIX 秒。未承認のまま期限を過ぎたら status='expired'
  status TEXT NOT NULL
    CHECK (status IN ('pending', 'approved', 'rejected', 'expired'))
);

-- 2. 個人が避けたい交差点・狭路セグメント（TTL付き・版跨ぎ再照合用）
CREATE TABLE avoidance_nodes_edges (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  target_type TEXT NOT NULL,              -- 'junction_transition' | 'edge' | 'bbox'
  graph_version TEXT NOT NULL,
  anchor_lat REAL NOT NULL,
  anchor_lon REAL NOT NULL,
  in_bearing_deg REAL,
  out_bearing_deg REAL,
  reason_tag TEXT NOT NULL,               -- 'unprotected_right' | 'narrow_stress' | 'complex_junction'
  is_hard_constraint INTEGER NOT NULL DEFAULT 0, -- 0: L2ソフト回避, 1: 停車中明示登録ハード制約
  hit_count INTEGER NOT NULL DEFAULT 1,
  last_triggered_at INTEGER NOT NULL,
  expires_at INTEGER,                     -- UNIX 時刻（永続）。ソフト回避は最終発火から180日で自動減衰・削除、
                                          -- ハード制約は NULL（期限なし）
  match_status TEXT NOT NULL DEFAULT 'matched'
    CHECK (match_status IN ('matched', 'needs_rematch', 'orphaned')),
                                          -- 版跨ぎ再照合の状態。
                                          -- orphaned のハード制約は S12 で通知する（黙って消さない）
  CHECK ((is_hard_constraint = 0 AND expires_at IS NOT NULL)
      OR (is_hard_constraint = 1 AND expires_at IS NULL))
);
CREATE INDEX idx_avoidance_expires ON avoidance_nodes_edges(expires_at);

-- 3. 学習提案候補の一時記録（到着後レビュー S12 の承認前は無効）
CREATE TABLE driving_feedback_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  trip_id TEXT NOT NULL,
  event_type TEXT NOT NULL,               -- 'route_rejected' | 'manual_detour' | 'voice_complaint'
  lat REAL NOT NULL,
  lon REAL NOT NULL,
  in_bearing_deg REAL,
  time_bucket TEXT NOT NULL,              -- 'morning' | 'day' | 'evening' | 'night'
  applied_to_profile INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
```
