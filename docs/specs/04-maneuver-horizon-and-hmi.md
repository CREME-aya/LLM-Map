# 04. `ManeuverHorizon` 単一ソースと 3 手先先読み HMI

> **対象フェーズ**: Phase 1（標準実装）〜 Phase 3
> **関連 ADR**: [ADR-0005](../adr/0005-no-hard-lock-and-maneuver-horizon-3.md), [ADR-0008](../adr/0008-dc-tick-single-event-entry.md)
> **意匠上の注意**: 「マツダ風」とは **3 つ先までの右左折・分岐を先読み提示する情報設計思想** のみを指し、他社の固有商標・ロゴ・具体的なアイコンやトレードドレスを模倣・転用しません。

---

## 1. 単一ソース原則（UI・音声・回避提案の一体化）

走行中の Android 画面（3 手先パネル）、音声優先度スケジューラ（P0〜P3）、および運転しやすさ回避提案のすべてが、Rust コアが生成する単一の不変スナップショット **`ManeuverHorizon` v1** のみを参照します。UI 側と音声側で Valhalla の `Directions` や座標を独自再解釈することを禁止します。

---

## 2. `ManeuverHorizon` v1 厳密データモデル

```text
MonoMs      = u64          // 単調時刻（ミリ秒）。JSON では整数（number）
U64Str      = u64          // JSON では十進文字列。OSM / Valhalla 由来の ID・revision など Web に渡りうる値だけ

RouteKey    = { trip_id, trip_revision: U64Str, route_revision: U64Str, graph_version }
ManeuverId  = { route_key, leg_index: u32, ordinal: u32 }
RouteAnchor = { leg_index: u32, leg_shape_index: u32, along_route_cm: u32 }   // u32 cm で最大約 42,900 km

Evidence<T> = confirmed { value: T, source_ref, observed_at? }
            | inferred  { value: T, rule_id, source_refs[] }
            | unknown   { reason: missing | conflict | direction_unclear | version_mismatch }

Arrow       = left | right | slight_left | slight_right | sharp_left | sharp_right
            | u_turn_left | u_turn_right | keep_left | keep_right | fork_left | fork_right
            | merge_left | merge_right | merge | exit_left | exit_right | exit
            | roundabout { exit_number?: u8 } | ferry_board | ferry_leave
            | straight_decision | arrive | generic

Crossing    = not_applicable | across_oncoming | no_crossing
Signal      = present | absent
Protection  = protected | unprotected

Maneuver = {
  id: ManeuverId,
  anchor: RouteAnchor,
  arrow: Arrow,
  junction_name?: Text,
  road_name?: Text,
  route_ref?: Text,
  side_of_traffic: left,
  crossing: Evidence<Crossing>,
  approach_signal: Evidence<Signal>,
  right_turn_protection: Evidence<Protection>,
  approach_width_mm: Evidence<u32>,
  departure_width_mm: Evidence<u32>,
  roundabout_leave_anchor?: RouteAnchor,
  advisory_flags: [Flag]
}

AvoidOffer = {
  offer_id: String,             // user_tap.target_id が指す ID（alt_route_id・override_id と同じ扱い。下記注記）
  alt_route_id: String,
  delta_eta_ms: i64,
  decision_deadline_monotonic_ms: MonoMs,
  summary: {                    // 文面は持たない（下記注記）
    template_id,
    delta_eta_ms,
    avoided: { unprotected_right: u8, narrow_m: u32 }
  }
}

HorizonEntry = {
  maneuver: Maneuver,
  group_id?: ManeuverId,
  from_vehicle_m?: u32,        // 1つ先主カード表示用（現在位置からの経路上距離）
  seg_from_prev_m?: u32,       // 2つ先・3つ先従カード表示用（1つ前の操作からの区間距離）
  eta_range_ms?: { earliest: u32, typical: u32, latest: u32 },
  action_required: bool,       // SA/PA や通過IC等の情報項目は false
  avoid_offer?: AvoidOffer
}

ManeuverHorizon = {
  schema_version: 1,
  route_key: RouteKey,
  frame_seq: u64,              // JSON では整数（number）。1 ずつ増える番号で 2^53 に届かない
  sampled_at_monotonic_ms: MonoMs,
  valid_until_monotonic_ms: MonoMs,
  status: tracking | uncertain | off_route | rerouting | finished,
  matched_progress_cm?: u32,
  upcoming: [HorizonEntry; 0..3],
  next_p1_deadline_monotonic_ms?: MonoMs,
  next_complex_window?: { start_monotonic_ms: MonoMs, end_monotonic_ms?: MonoMs },
  refs: { next?: JunctionRef, last?: PassedRef }   // frame_seq と同じ版。ref_table の正本
}

JunctionRef = { ref, maneuver_id: ManeuverId, kind, deadline_mono_ms: MonoMs }
PassedRef   = { ref, maneuver_id: ManeuverId, passed_at_mono_ms: MonoMs, passed_along_cm: u32 }
```

- **`refs`**: L3 命令コードの `jp=next` / `jp=last`（[03 §4.1](03-routing-and-two-layer-control.md)）が参照する対象表（ref_table）の正本。`frame_seq` と同じ版として発行し、他所で別の参照表を持たない。`JunctionRef` は次の難所（`jp=next`）で、締切 `deadline_mono_ms` を持つ。`PassedRef` は通過済みの直近難所（`jp=last`）で、通過時刻 `passed_at_mono_ms` と通過位置 `passed_along_cm` を持ち、`jp=last` の範囲判定（03 §4.1）に使う。
- **`AvoidOffer.summary`**: 文面そのものは持たない。表示・発話の文面は `template_id` と数値から **Kotlin のテンプレートで生成** し、**画面と音声で同じ文面** を使う。SLM は関与しない。
- **数値・列挙の JSON 表現**: `U64Str`（OSM / Valhalla 由来の ID・revision など Web に渡りうる `u64`）だけを **十進文字列** で受け渡し、`MonoMs`（単調時刻）やその他の数値は JSON の **整数（number）** とする。列挙値は JSON 上 **snake_case**（`tracking`・`across_oncoming` 等）。
- **`AvoidOffer.offer_id`**: 回避チップを指す ID。`user_tap.target_id` は回避チップならこの `offer_id`、それ以外は `override_id` / `confirm_screen_id` を指す（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。
- **`dc_tick` との関係**: `ManeuverHorizon` は `dc_tick` の `TickOutput.horizon?` として返る。`horizon` は **`frame_seq` が変わったときだけ** 返し、変化がなければ省略できる。`TickOutput` はほかに `voice_cmds` / `ui_cmds` / `planner_cmds` / `next_wake_mono_ms` / `log` を持つ。`next_wake_mono_ms` は **新しい入力がなくても出力が変わり得る最も早い単調時刻**（探索の監視期限を含む）で、Kotlin はそれまでに `dc_tick` を呼ぶ（早めに呼ぶのは可）。待つものがなければ **null** で、その場合は Fix が届くたびの tick だけでよい。型定義の全文は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md) を正本とする。

### 2.1 正規化・距離軸・グルーピング規則
1. **Trip と Directions の突き合わせ**: C++ 側で同一 leg の Trip と Directions（`begin/end_shape_index`, `begin/end_path_index`）を照合。Odin が内部辺を畳む箇所は位置対応を検証し、範囲外や不整合時は誤誘導せずデータエラーを返す。未知 maneuver type は `generic` とし左右推論で補完しない。
2. **独立の累積距離軸**: leg ごとの polyline にアンカーを投影し `along_route_cm` を作成。別軸の `maneuver.length` は使わない。ETA は辺ごとの走行予測と端点時刻から補間し、GPS の瞬間速度だけで計算しない。
3. **対象操作の選別**:
   - 出発・道なりの通常直進・単なる道路名変更・仮想経由点は除外し、分岐の選択が必要な直進のみ `straight_decision` として残す。
   - 環状交差点（ラウンドアバウト）は進入〜退出を画面上 **1 枠** に束ねつつ、内部では両アンカーの案内時刻を保持する（不一致時は出口番号を補完しない）。
   - 都道府県境で路線名が変わっても同一操作の `ManeuverId` は再発行しない。
4. **連続分岐の `group_id` と枠消費規則**:
   - 隣接操作が **都市 120 m 以下（高速 200 m 以下）または最早 ETA 差 12 秒以下** で間に実停車地点がない場合、同じ `group_id` を付与（初期上限は先頭から最大 2 操作）。数値は暫定初期値（PoC で校正）、正本は [07 §1](07-parameters-and-poc-gates.md)。
   - **画面と音声**: 画面では **別の操作として 3 枠内の枠を消費させ（2 手目まで見える安心感を維持）**、1 つ先カード下段に `└ すぐ左 ↰` の連結バッジを添え、音声スケジューラも必要に応じて「約 200m 先を左折、そのすぐ先 80m を右折」と結合発話する。音声側は **`HorizonEntry.group_id` のみを参照** し、距離・秒数の閾値を独自に持たない（[05 §5](05-inference-tier-and-audio.md)）。終了間際は最大 3 枠、到着時は 1 枠。

---

## 3. 現在地整合・再探索・版切替の原子性

Android の単一 `NavigationSession` actor が、GNSS の受け取り・経路コミット・音声開始/停止を直列管理し、UI と音声は常に同じ `route_key`・`frame_seq` を参照します。**経路上の位置合わせは Rust が `dc_tick` の中で行います**（下記 §3.1）。

### 3.1 経路本体 `route_bundle` と経路上の位置合わせ（Rust）
- **範囲**: Rust が行うのは **確定した経路の上での位置合わせだけ**。道路網全体へのマップマッチングは行わず、タイルや道路網を Rust に持ち込まない（IO を持たない原則）。経路から外れたと判定したら、Rust は現在位置を付けて `reroute` の `start` を出し、道路網を使う探索はこれまでどおり Valhalla（C++）が受け持つ。
- **経路本体**: `dc_route_commit` で一度だけ渡す。JSON の形の正本は `schema/route-bundle.schema.json`。構造は `legs: [{ leg_index, shape, edges, maneuvers }]`。累積距離は leg ごとの値と経路全体の `along_route_cm`（u32 cm）の両方を持つ。`edges` は `edge_id`（U64Str）・区間・`length_cm`・`cum_time_ms`（u32）・`flags`。`maneuvers` は §2 の `Maneuver` を `$ref` で使い、別の型を作らない（正規化は §2.1 のとおり C++）。残り ETA は `edges.cum_time_ms` と位置合わせで求めた現在位置から計算する。
- **座標の契約（三角関数を Rust に入れない）**: 形状は **チャンクごと**（目安 20 km、暫定）に基準点 `origin_e7` と **整数の換算係数** `cm_per_e7_lat` / `cm_per_e7_lon`（固定小数点）を持ち、点はそのチャンクの基準点からの **整数 cm** で表す。係数は経路を作る側の C++ が `cos` を使って一度だけ計算する。`TickInput.fix` の緯度経度は **e7 の整数**（Kotlin が `Location` の double を一度だけ丸める）で、Rust は基準点を引いて係数を掛けるだけで平面に直す（整数の四則演算だけなので x86 と arm64 で同じ結果）。チャンクの境目では Rust が両隣のチャンクで変換して近い方を採る（判断はすべて Rust）。固定小数点の決まりごと（暫定）：
  - 係数は 1e-7 度あたりの cm を **Q16（u32）** で表す（緯度方向 ≈ 1.1132 cm → 約 72,955。経度方向はこれに `cos(緯度)` を掛けた値。C++ が一度だけ計算）。
  - 変換は `dx_cm = round_q16((lon_e7 − origin.lon_e7) × k_lon_q16)`、計算途中は **i64**。丸めは `(v + 2^15) >> 16`（算術シフト。ちょうど 0.5 は +∞ 方向）。
  - Kotlin は `Location` の double を `Math.round(deg × 1e7)` で e7 に丸める（決定論が要るのは e7 以降）。
  - 距離の二乗と内積は i64、平方根が要るところは整数平方根（切り捨て）。位置合わせと Horizon の計算では f32 / f64 を使わない（テストで確認）。
  - **方位も三角関数を使わない**：各線分の進行方位は C++ が整数（センチ度 `bearing_cdeg`）で `route_bundle` に入れ、Fix の `bearing` は Kotlin が `Math.round(deg × 100) % 36000` で整数センチ度（0〜35999）に丸めて渡す（359.996° のような値が 36000 にならないよう、丸めた後に 36000 で割った余りを取る）。Rust は整数の角度差（0〜18000）で比べるだけ。
  - チャンクの有効範囲は、そのチャンクの点を囲む長方形に 1 km の余白を足した範囲。境目では両隣で変換し、横方向の距離が小さい方、同じなら経路上の距離が短い方を採る。
  - 誤差の見積もり（推測）：20 km のチャンクで計算途中の値は約 1.3×10^11（i64 に収まる）、係数の丸め誤差はチャンクの端で 15 cm 前後。区間境界で累積距離が連続することをテストで確認する。
- **位置合わせの手順**: 通常は前回の進捗の周辺の線分だけを調べる。トンネル復帰などで見失ったときは、経路の線分の空間索引から候補を引いて再捕捉する。並行道路などで候補を絞れないときは、進捗と残り ETA を **未確認** とし（`status=uncertain`）、3 手先の新規提案・P2/P3 を抑え、計画の外の探索の距離区分は `long` になる（[03 §3.1](../specs/03-routing-and-two-layer-control.md)）。
- **精度の悪い Fix**: `acc_cm` が `max_acc_cm` を超える Fix は横のずれの判定に使わず、逸脱の連続カウントを **0 に戻す**（逸脱の確定は「有効な範囲外 Fix が連続 `confirm_fixes` 回」のときだけ）。その代わり、精度の悪い Fix が間に挟まり続けると逸脱の確定は遅れる（既知のトレードオフ。Gate 7 で記録）。そういう Fix が `confirm_fixes` 回続いたら `status=uncertain`。`bearing_cdeg` が 0〜35999 の外なら `E_SCHEMA` で拒否する。
- **外れ判定の閾値**（横方向の許容・進行方向の許容・確定に要る連続 Fix 数・測位精度の上限）は経路ごとではなく方針値として [07 §1](07-parameters-and-poc-gates.md) に置き、`dc_session_new` の設定で渡す。
- **大きさ（根拠：合成データでの概算、sol6.2さん）**: 5,000 点・1,000 辺・100 操作で約 0.26 MB、20,000 点・5,000 辺・400 操作で約 1.20 MB。実機での転送時間・メモリは未計測。C++ 生成・JNI 転送・Rust の解析と索引構築・確定までを、採点入力の 300 ms 目安とは別に測る。長距離の結果が出るまでは JSON で進める。
- **浮動小数点の決定論**（推測）: `cos` などの標準ライブラリ関数は実装により最後の桁がずれ得るため、Rust では使わない。実機と PC のリプレイ結果が一致するかは Gate 0 で確認する。

- **操作通過条件**: 単純な円形半径や GPS 速度だけで通過判定せず、**精度・進行方向・連続性を満たす次の辺上の高信頼マッチ（例：2 サンプル連続）** で通過確定。短い後方ジッタで過去操作を復活させない。
- **状態ごとの抑制ルール**:
  - 鮮度初期上限は **2.5 秒**。高架・並行道路・トンネル等で確信不足時は `uncertain` とし、距離/秒数更新・P2/P3 音声・3 手先の新規提案を抑制。
  - `off_route` / `rerouting` 時は旧方向案内を保留し「経路を確認中」を表示（再探索の応答は終端イベント `search_finished`（[03 §3.1](03-routing-and-two-layer-control.md)）で受け、**`outcome=ok` かつ `route_key`（`trip_revision` / `route_revision` / `graph_version` を含む）が現行一致、`status=tracking` のときのみ** 原子コミット。cancel 後に届いた `ok` は `discarded_late`、`engine_stuck` の後に届いた `ok` は `discarded_after_stuck` として記録し、どちらも結果は使わない。`engine_stuck` の後も逸脱再探索が必要なら直後に新しい `start` を出す）。
  - `finished` 時は先読み配列 `upcoming` を空にする。
  - `next_p1_deadline_monotonic_ms` には画面が省く中間案内ステップや連続グループ内側の締切も含める。

---

## 4. 3 手先先読み HMI（`ManeuverLadder`）デザイン仕様

### 4.1 視覚階層と 1.5 秒ちら見原則（計 10 要素以内）

| 階層 | 表示上限（要素数） | 要素とサイズ目安（PoC 校正値） | 更新・表示ルール |
|---|---|---|---|
| **1 つ先（主カード）** | **4 要素まで** | ①大矢印（120〜160 dp）②残距離（48 sp 太字）③交差点/方面名（20 sp・12 字省略）④バッジ最大 2 個（＋連続時のみ `すぐ左`） | 1km 以上: 0.1km 刻み / 300m〜1km: 50m 刻み / 300m 未満: 10m 刻み。**表示更新は 1 秒に 1 回まで**（ちらつき防止）。 |
| **2 つ先・3 つ先（従カード）** | **各 3 要素まで** | ①小矢印（40 dp）②前項目からの区間距離（18 sp）③注意アイコン最大 1 個 | 交差点名は表示しない。区間距離を接続線上に配置。 |
| **運転しやすさ安心 1 行** | 補足 1 行（任意） | 「この先 5 km 右折なし」「次の右折：信号あり」（16 sp） | 3 手先範囲内に高リスク操作がない時のみ表示。 |

- **到達時間（ETA）ベースの強調**:
  - 一般道：到達 **30 秒前** で主カード枠を「準備」強調、**8 秒前** で「まもなく」色強調＋音声案内。
  - 高速道路：到達 **60 秒前** で「準備」、**20 秒前** で「まもなく」（標識に合わせて 2km / 1km / 500m を重視）。
- **色覚配慮バッジ（形＋文字＋確からしさ区別）**:
  - 信号なし右折（琥珀三角 `▲信号なし`）、狭路進入（縦二重線 `狭路`）、短い合流（合流記号 `合流`）、車線位置（`左車線` 等）。
  - `confirmed` は **塗りつぶし**、`inferred`/`unknown` は **輪郭線＋`?`** で視覚区別。
- **アニメーション規則**: 通過時は全体を 1 段上へ **250 ms スライド**（跳ねなし）、再探索時は **150 ms クロスフェード**。注意を引く過度な点滅は禁止。

### 4.2 画面レイアウト（縦置き / 横置き / 高速道路モード）

```text
【縦画面（標準・走行中ソフトモード）】
┌──────────────────────────────┐
│ ┌──────────────────────────┐ │
│ │   ↱        320 m          │ │ ← 1つ先（主）：上部約 35%
│ │  (大矢印)   本町三丁目      │ │
│ │           [▲信号なし][狭路] │ │
│ │           └ すぐ左 ↰        │ │ ← 連続分岐時のみ
│ └──────────────────────────┘ │
│   ─ 1.2km ─▶ ┌────┐ ─ 4.5km ─▶ ┌────┐│ ← 2つ先・3つ先（従）：約 10%
│              │ ↰  │            │ ↱ ⚠ ││
│              └────┘            └────┘│
│            [回避 +2分]        ← リスク箇所がある場合のみ直下に提示
├──────────────────────────────┤
│                              │
│          地 図               │ ← 約 45%（ロックなし：パン・ズーム自由）
│            ▲(自車)           │
│                    [◎現在地]  │ ← パン時のみ表示・10秒後自動復帰
├──────────────────────────────┤
│ 到着 14:32 ・ 残り 18km ・ 🗾  │ ← 約 7%（下部情報帯）
│ ▔▔▔ (ドロワーつまみ)           │ ← 全操作リスト / 簡易音声操作
└──────────────────────────────┘

【横画面（ホルダー横置き：左右反転設定可）】
┌───────────────────────┬──────────────────────────────────┐
│  ↱      320 m          │                                  │
│ (大矢印) 本町三丁目      │                                  │
│ [▲信号なし][狭路]       │            地 図                 │
│ └ すぐ左 ↰              │              ▲                   │
├───────────────────────┤                                  │
│ ↰   1.2km               │                                  │
│ ↱ ⚠ 4.5km  [回避 +2分]  │                       [◎現在地]  │
├───────────────────────┤                                  │
│ 14:32着 ・ 18km ・ 🗾    │ ▕ドロワー（右端・15秒自動閉）      │
└───────────────────────┴──────────────────────────────────┘
  左側約 38%：3手先パネル（縦積み一覧で視線移動を短縮）
```

- **高速道路自動変形 (`motorway`/`trunk` 有料区間時)**:
  - 操作必須ノード（JCT・降りる出口：`action_required = true`）には矢印を付け、情報ノード（SA/PA・通過 IC：`action_required = false`）は矢印なし灰色で表示。リストには必ず操作必須ノードを最低 1 件残す。

### 4.3 3 手先連動の「運転しやすさ回避チップ」と強制ロックなし干渉防止
- **回避チップの発火と締め切り**:
  1. 3 手先（または到達約 120 秒前）に `信号なし右折` / `狭路` が入ると、裏で決定論 L1/L2 によりその操作を回避する迂回経路を計算。計算予算は **1.5 秒** とし、**締切（回避分岐の 20 秒前）＋計算予算 以上手前** のときだけ計算を開始する。探索は RoutePlanner の単一優先度キュー（[03 §3.1](03-routing-and-two-layer-control.md)）に載り、優先順は 逸脱再探索 > 明示計画 > **回避チップ** > 比較探索 の **3 番目**。
  2. 増加時間が迂回上限（[03 §3.2](03-routing-and-two-layer-control.md) の式。走行中は `base_eta` = **残り ETA**）以内かつ **締切より手前** なら、対象カードに `[回避 +2分]` を提示し、到達約 60 秒前に 1 回だけ音声予告。
  3. 1 タップ（76 dp 以上、`user_tap{target_id: offer_id, action: avoid}`）または音声「回避」でクロスフェード切替（5 秒以内の「戻して」で取消可。L3 命令コードでは `CXL`、[03 §4.1](03-routing-and-two-layer-control.md)）。締切経過後は即座に消す。音声「回避」で確定するのは **Rust 辞書照合で一致した `dict_norm`** の場合のみで、確定入口はこれと `user_tap` の 2 つだけ（SLM の出力では確定しない。[ADR-0007](../adr/0007-l3-one-line-command-and-confirm-separation.md)）。
  4. 出発前比較でユーザーが承知して選んだ箇所への回避提案は出さず（バッジのみ表示）、音声予告は **1 走行最大 5 回** に制限。
  5. 表示・締切・取消の判定はすべて `dc_tick`（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）で行う。
  6. **停止遅延（探索 cancel から実停止まで）を実測するまでは、走行中の回避チップ先読みは無効（既定）** として PoC を進める。
  - 数値（1.5 秒・20 秒・5 回）は暫定初期値（PoC で校正）、正本は [07 §1](07-parameters-and-poc-gates.md)。
- **強制マップ操作ロックなしでの干渉防止**:
  - 地図のパン・ズームは地図領域のみに効き、3 手先パネルは常時固定（10 秒無操作で現在地復帰）。
  - 走行中の 3 手先パネルスワイプは無効化し、タップ時は助手席向けの全操作リスト（行高 76 dp、15 秒無操作自動閉）を表示。
  - ドロワー展開時も地図領域が縮むのみで 3 手先パネルの視認性を維持。文字入力欄への自動フォーカス禁止。
