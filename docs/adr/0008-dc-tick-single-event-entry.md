# ADR-0008: dc_tick を唯一のイベント入口に

- **ステータス**: Proposed
- **日付**: 2026-11-24
- **関連**: [ADR-0002](0002-narrow-pure-rust-core.md) / [ADR-0004](0004-isolated-inference-and-hardware-tiers.md) / [ADR-0005](0005-no-hard-lock-and-maneuver-horizon-3.md) / [ADR-0007](0007-l3-one-line-command-and-confirm-separation.md) / [02 §2.1](../specs/02-architecture-and-stack.md) / [03 §3.1・§5](../specs/03-routing-and-two-layer-control.md) / [04 §2・§4.3](../specs/04-maneuver-horizon-and-hmi.md) / [05 §3.1・§3.2](../specs/05-inference-tier-and-audio.md) / [07 §2](../specs/07-parameters-and-poc-gates.md)

## コンテキスト
走行中のナビ本体には、測位（Fix）、TTS・ASR の進み具合、推論プロセスの結果と障害、ユーザーのタップ、熱状態、メモリ不足、オーディオフォーカス、通話状態、経路探索の完了といったイベントが、ばらばらの時刻に届く。v1.1 までの docs は、探索の終端を `reroute_result` / `search_cancelled` という別々のイベントとして扱い、どのイベントを、どの時刻を基準に、どの順序で Rust に渡すかを 1 か所で決めていなかった。

[ADR-0002](0002-narrow-pure-rust-core.md) は Rust コアを「IO・DB・HTTP・非同期・独自スレッドを一切持たない狭い純粋 Rust コア」とし、境界を「粗粒度 ≤ 15 関数・版付き UTF-8 JSON」と定めている。ただし、走行中のセッション状態（探索枠、提案の期限、音声スケジューラ）を誰が持つのか、どの時計で期限を判定するのかは書いていない。

## 決定
1. **Rust C ABI v1 の 12 関数**: `dc_abi_version` / `dc_buf_free` / `dc_session_new` / `dc_session_free` / `dc_plan_candidates` / `dc_rank` / `dc_validate_override` / `dc_trip_score` / `dc_route_commit` / `dc_tick` / `dc_avoid_offer_eval` / `dc_exchange_validate`（呼び出し規約の詳細は [02 §2.1](../specs/02-architecture-and-stack.md)）。
   - 入力 `dc_in{ptr,len}` は呼び出し側が持つ（関数が戻るまで有効）。出力 `dc_out` は Rust が確保し、`dc_buf_free` で解放する。返り値は `{ok, data | error{code,msg}}` で、panic は `catch_unwind` で捕まえて `E_PANIC` にする。
   - 呼び出すのは `NavigationSession` actor 1 本だけ。
   - `dc_validate_override` の結果 `Applied` / `Rejected` は型名で、JSON では判別タグ `"result": "applied" | "rejected"`（snake_case）として出す（Rust 内部の状態名 `Idle` / `Running` / `CancelRequested` は JSON に出ないので PascalCase のまま）。
2. **走行中のイベントは `dc_tick` からしか入れない**。型の分担は次のとおり。**JSON の形（フィールド名・型・必須/任意・列挙値・判別タグ）の正本は `schema/tick.schema.json`**（テストで機械検証する）。**各フィールドの意味と決定の理由の正本は本 ADR**。下の型定義は本 ADR での要約で、形が `schema/tick.schema.json` と食い違ったら schema を正として本 ADR を直す。他の文書には要点とリンクだけを置く。
   - 判別タグ：`Event` は `kind`、`PlannerCmd` は `cmd`、`UiCmd` は `ui` で種別を表す（例：`{"at_mono_ms":…, "kind":"tts_started", "token":…}`）。

```
TickInput  = { now_mono_ms, fix?: Fix, events: [Event] }          // events は at_mono_ms 昇順（Rust が検証）
Fix        = { at_mono_ms, lat_e7, lon_e7, acc_cm, bearing_cdeg?, speed_cm_s? }   // すべて整数（e7・cm・センチ度・cm/s。Kotlin が一度だけ丸める。04 §3.1）
Event = { at_mono_ms } & (
  | tts_started{token} | tts_done{token} | tts_stopped{token}
  | asr_started{utterance_id}
  | asr_result{utterance_id, text_class: dict|natural, dict_norm?: string}
  | asr_failed{utterance_id}
  | override_ready{utterance_id, override_id?, status: ok|timeout|error, confirm_screen_id}
  | user_tap{target_id, action: confirm|avoid|undo}   // target_id = offer_id（回避チップ）| override_id | confirm_screen_id
  | thermal{status: none|light|moderate|severe|critical|emergency|shutdown}   // Rust で critical 以上を 1 つに
  | low_memory | inference_down{reason} | inference_up{tier}
  | audio_focus_lost | call_state{active}
  | search_finished{req_id, route_key, graph_version, outcome, facts_ref?, engine_ms}
  | engine_reset )
TickOutput = { horizon?, voice_cmds, ui_cmds, planner_cmds, next_wake_mono_ms: u64 | null, log: [LogEvent] }
```

   - `*_mono_ms`（`MonoMs`）は JSON の **整数（number）**。十進文字列（`U64Str`）にするのは OSM / Valhalla 由来の ID・revision など Web に渡りうる `u64` だけ。列挙値は JSON 上 **snake_case**。
   - `thermal.status` の 7 段階は `THERMAL_STATUS_*` 定数を記憶で書いたもので、**未確認（Gate 0 で確認）**。
   - `events` が `at_mono_ms` の昇順になっていない場合、Rust は **並べ替えず**、前の値に合わせて `clock_regression` を記録する。
3. **出力**
   - `horizon` は `frame_seq` が変わったときだけ返す（[04 §2](../specs/04-maneuver-horizon-and-hmi.md)）。
   - `planner_cmds: start{req_id, priority, distance_class, route_key, graph_version} | cancel{req_id} | commit{req_id, route_key} | rank{plan_request_id, facts_refs, reason: all_done | budget_expired}` は Rust が出し、Kotlin が実行する。`commit` は逸脱再探索の結果を確定する指示で、Kotlin が `dc_route_commit` を呼ぶ（回避チップの結果には、利用者が同意するまで出さない）。`rank` を受けた Kotlin が `dc_rank` を呼ぶ（Rust は `CandidateFacts` を持たないため）。`search_finished.facts_ref` は採点前の `CandidateFacts` への参照（意味の詳細は [03 §3.1](../specs/03-routing-and-two-layer-control.md)）。
   - `voice_cmds`: `speak{token, priority: p0|p1|p2|p3, template_id, params}` | `stop{token}`（判別タグ `voice`）。文面は持たず、Kotlin が画面と同じテンプレートで組み立てる（ADR-0007）。`stop` の後、Kotlin は `onStop` を待たずに次の P1 を出してよい（[05 §5](../specs/05-inference-tier-and-audio.md)）。
   - `ui_cmds`（判別タグ `ui`）: `show_confirm{confirm_screen_id, override_id, expires_at_mono_ms}` | `dismiss{target_id, reason: expired|cancelled|route_changed|superseded}`。`superseded` は新しい提案に差し替わったときで、承認は引き継がない。3 手先カードと回避チップ（`HorizonEntry.avoid_offer`）は `horizon` だけで伝え、`ui_cmds` には重ねて出さない（単一ソース、[04 §1](../specs/04-maneuver-horizon-and-hmi.md)）。「経路を確認中」も `ui_cmds` では出さず `horizon.status` で表す（旧 `show_checking_route` は廃止）。回避チップは **表示は `horizon` だけで行い、取り下げ（締切切れ・経路変更）だけは `dismiss` で知らせる**（Kotlin が画面と音声の後始末をするため）。
   - `next_wake_mono_ms` は、**新しい入力がなくても出力が変わり得る最も早い単調時刻**（探索の監視期限を含む）。Kotlin はそれまでに `dc_tick` を呼ぶ（早めに呼ぶのは可）。待つものがなければ **null** で、その場合は Fix が届くたびの tick だけでよい。
4. **状態は持つが、IO・スレッド・時計は持たない**: Rust はセッション（`dc_session_new` / `dc_session_free`）の中に状態を持つ。IO・スレッド・時計は持たない。
5. **単調時刻の規約**
   - Kotlin は呼び出しのたびに `SystemClock.elapsedRealtimeNanos()/1e6` を `now_mono_ms` として渡す。Fix の時刻は `Location.getElapsedRealtimeNanos()` から取る。
   - 時刻が巻き戻ったら前の値に合わせ、`clock_regression` を記録する。GPX リプレイでは合成時計を使う。
   - セッション内の締切（永続しない）は単調時刻で判定する。180 日減衰の `expires_at`（永続する）は UNIX 時刻にする（[03 §5](../specs/03-routing-and-two-layer-control.md)）。
6. **探索の終端イベントは `search_finished` 1 つにまとめる**（詳細は [03 §3.1](../specs/03-routing-and-two-layer-control.md)）。
   - `reroute_result` / `search_cancelled` は廃止する。1 回の探索につき `search_finished` は必ず 1 回だけ返る。`outcome` は `ok|cancelled|error`（`timeout` は無い）。
   - Rust の状態は `Idle → Running → CancelRequested → Idle`（監視期限を超えたら `engine_stuck` を経て、実際の `search_finished` で Idle）。`search_finished` が届くまで次の `start` を出さない。
   - cancel した後に `ok` が返ってきたら `discarded_late` として記録し、結果は使わず探索枠だけ空ける。結果を使うのは、`outcome=ok` かつ `route_key` が今の経路と一致し、かつ status が `tracking` のときだけ。
   - ネイティブ呼び出し開始からの監視上限（短距離 5 s / 長距離 10 s、暫定。[07 §1](../specs/07-parameters-and-poc-gates.md)）の超過は **Rust が `now_mono_ms` と `start` 時刻から検出** し、`engine_stuck`（「経路を確認中」と表示）にする。Kotlin は timeout を通知せず、`next_wake_mono_ms`（監視期限を含む）に合わせて tick を呼ぶだけ。探索枠は占有したままにし、ネイティブ呼び出しが実際に戻った時点で `search_finished` を 1 回返し、そのとき探索枠を解放する。`ok` で戻っても結果は捨て `discarded_after_stuck` と記録する。逸脱再探索がまだ必要なら直後に新しい `start` を出す。新しい Actor を作れただけでは再開しない。`engine_reset` イベントを用意する。
7. **ログに発話の生の文字列を残さない**: 残すのは `text_class` と辞書 ID だけ。原文は、ハーネスで明示的に有効にしたときだけ残す。
8. 走行中の確定入口（`asr_result` の `dict_norm` と `user_tap`）の意味は [ADR-0007](0007-l3-one-line-command-and-confirm-separation.md) に従う。コールド経路の SLM 結果を出すか捨てるか（`route_key` が一致し確認画面が有効なときだけ出す）も `dc_tick` で判定する（[05 §3.2](../specs/05-inference-tier-and-audio.md)）。

### ADR-0002 との差分
ADR-0002 の本文は書き換えない（Accepted）。本 ADR で次の点を加える・はっきりさせる。
- ADR-0002 の「IO・DB・HTTP・非同期・独自スレッドを一切持たない」はそのまま守る。そのうえで **「時計も持たない」** を加える（時刻はすべて Kotlin が渡す）。
- ADR-0002 は Rust がセッション状態を持つかどうかを書いていない。本 ADR では、**Rust は `dc_session_*` のセッション状態（探索の状態機械、提案の期限など）を持つ** と決める。したがって本 ADR は、ADR-0002 の「純粋」を「IO・スレッド・時計を持たない」という意味で読み、セッション状態を持つことはこれに反しないとする（この読み方は本 ADR での解釈で、正本に文章として書かれてはいない）。
- ADR-0002 の「粗粒度 ≤ 15 関数・版付き UTF-8 JSON」は、C ABI v1 の 12 関数として具体的に決めた（上限の範囲内）。
- ADR-0002 の「Rust ↔ C++ 直結禁止」は変わらない。C++ が採点入力の JSON を 1 回だけ作り、Kotlin はそれを `ByteArray` のまま Rust に渡す（作り直さない。[02 §2.1](../specs/02-architecture-and-stack.md)）。

## 検討した代替案
- **探索の終端を `reroute_result` / `search_cancelled` に分ける案（v1.1）**: 正本はこれを `search_finished` 1 つにまとめ、「1 探索につき必ず 1 回」を保証することにした。cancel した後に `ok` が来る場合（`discarded_late`）や、`engine_stuck` の後に実際の戻りが来る場合（`discarded_after_stuck`）も、同じ 1 回の終端で探索枠を空けられる。これ以外の理由は正本に書かれていない。
- **ADR-0002 の「≤ 15 関数の粗粒度 JSON 境界」だけを決め、Rust にセッション状態もイベント入口も持たせない案**: 正本は Rust に状態を持たせ、走行中のイベントを `dc_tick` 1 か所から入れると決めた。この案を退けた理由は、正本に文章として書かれていない。
- **Kotlin が C++ の結果 JSON を組み直して Rust に渡す案**: 正本は「再シリアライズしない」と決めた。性能が問題になったときの見直し方は再評価条件に書く。組み直さない理由そのものは、正本に書かれていない。

## 影響とトレードオフ
- **利点**: イベントの順序と時刻が 1 本の入力列にまとまるので、GPX リプレイ（合成時計）で走行中の挙動を決定論的に再現できる。
- **利点**: 探索 1 回につき終端が必ず 1 回返り、それが届くまで次の `start` を出さないので、同時に走る探索は 1 本に抑えられる。ネイティブ呼び出しが戻らない間は、探索枠を占有したまま `engine_stuck` として表に出る。
- **制約**: Rust がセッション状態を持つので、`dc_session_new` / `dc_session_free` の寿命管理と、`NavigationSession` actor 1 本だけから呼ぶ規律を守る必要がある。
- **制約**: Kotlin は `next_wake_mono_ms` に合わせて `dc_tick` を呼び直す必要がある。時刻の取り方（`elapsedRealtimeNanos` と Fix 時刻）を揃えることも、Kotlin 側の責任になる。
- **制約**: ログに発話の原文が残らないので、認識誤りを調べるにはハーネスで明示的に有効にする必要がある。

## 再評価条件
- **Gate 0**: valhalla-mobile 0.6.2 の割り込みパッチ（割り込みコールバックと `cancel(req_id)`）で、cancel した後に同じ Actor で次の探索が成功するか、`engine_reset` ができるかを確認する。パッチできなければ素の 0.6.2 で先読みを無効にし、それもできなければ LAN 越しにホストの Valhalla を使う（正本の退路）。`engine_reset` ができない場合に `engine_stuck` からどう戻すかの **対応方針は未決**。
- **Gate 0**: `THERMAL_STATUS_*` の段階数が記憶と違った場合は、`thermal.status` の列挙を見直す。どう直すかの **対応方針は未決**。
- **Gate 7**: 共通テストデータ（`start{r1, avoid_chip}` → 逸脱で `cancel{r1}` → `search_finished{r1, ok}` を捨てて `start{r2, reroute}` → `search_finished{r2, ok}` を原子コミット。派生として、t2 が来ない → 監視期限超過を Rust が検出 → `engine_stuck` → 実際の戻りの後に `search_finished`（`discarded_after_stuck`）→ r2）で、同時 Running ≤ 1、r1 由来の表示・発話 0 を確かめ、(t2−t1)・(t3−t1) を記録する。満たせない場合の **対応方針は未決**。停止遅延を実測するまでは、走行中の回避チップ先読みを無効にして進める。
- **Gate 4**: 共通テスト（発話中の提案差し替え、承認直前の逸脱、表示後の締切超過）で経路変更 0・古い案内発話 0 を確かめる。コールド結果を捨てる判定を含む。満たせない場合の **対応方針は未決**。
- **受け渡し性能**: 短距離 7 候補で受け渡しと解析の p95 が 300 ms を超えたら内訳を調べ、コピーが主な原因なら `DirectByteBuffer` と比べる（計測点は JNI ラッパー側）。
- 関数の数や非同期コールバックが増えた場合の `UniFFI` 移行は、[ADR-0002](0002-narrow-pure-rust-core.md) の再評価条件に従う。
