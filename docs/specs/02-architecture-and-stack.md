# 02. アーキテクチャと確定技術スタック

> **対象フェーズ**: Phase 1〜3 全体
> **関連図**: [技術スタック全体構成図](../assets/tech-stack.png)
> **関連 ADR**: [ADR-0002](../adr/0002-narrow-pure-rust-core.md), [ADR-0003](../adr/0003-valhalla-on-device-and-japan-packs.md), [ADR-0004](../adr/0004-isolated-inference-and-hardware-tiers.md), [ADR-0006](../adr/0006-gms-optional-and-history-preemption.md), [ADR-0007](../adr/0007-l3-one-line-command-and-confirm-separation.md), [ADR-0008](../adr/0008-dc-tick-single-event-entry.md)

---

## 1. 確定技術スタック一覧

Round 5〜6 の 3 者協議により、「1 人＋AI コーディングでの完走可能性」「ミドルレンジ実機での動作安定性」「Web/パイプラインとの境界明確化」を両立する以下の 1 案へ統一しました。

| 層 | 採用技術 | 責務・採用理由 |
|---|---|---|
| **Android UI / OS 層** | **Kotlin** ＋ **Jetpack Compose (Material 3)** ＋ **Navigation Compose** ＋ **ViewModel / StateFlow** ＋ **手動 DI (`AppContainer`)** | 画面描画（3 手先パネル含む）、NavigationSession ライフサイクル、Android サービス連携。DI フレームワーク（Hilt/Koin）は規模に見合わず不採用とし手動 DI に簡素化。 |
| **地図描画 (Android)** | **MapLibre Native Android** ＋ ローカル **PMTiles** | OSMF 公開タイルへの負荷集中を避け、自前配布 PMTiles を直接描画。Web 側とスタイル JSON を共通化。 |
| **ローカル DB (Android)** | **AndroidX `sqlite-bundled` (`BundledSQLiteDriver`)** ＋ **Room**（アプリ状態用）＋ 生 SQL（読み取り専用パック用） | **一次ソース確認済み**: AndroidX 同梱ビルドフラグに `ENABLE_FTS5` と `ENABLE_RTREE` の両方が含まれており、OS 搭載 SQLite の端末差を回避。DB を Rust 側に持たせず二重リンク衝突を排除。<br>**個人プロファイル DDL**（`profile_weights` / `profile_proposals` 等）は CHECK 制約を使うため **Room エンティティではなく生 SQL / SQLDelight** で定義（DDL 正本は [03 §5](03-routing-and-two-layer-control.md)）。 |
| **位置情報・FGS・DL・音声** | ・位置: OS 標準 **`LocationManager` (`GPS_PROVIDER`)** 基準（`LocationSource` 抽象化・**Phase 1 GMS 非依存**）<br>・サービス: `location` タイプ ForegroundService<br>・DL: **OkHttp**（Range/206 再開・SHA-256）＋ **WorkManager** / **JobScheduler UIDT** (API 34+)<br>・音声: OS 標準 **`TextToSpeech`** ＋ **`SpeechRecognizer` (`createOnDeviceSpeechRecognizer`)**<br>・監視: **ADPF `PowerManager`** Thermal API ＋ `MemoryInfo` | 屋外普通車ナビでは標準 GPS + 経路マップマッチングで成立し、GMS 非搭載端末との互換を維持（必要性が実測された場合のみ Play フレーバーへ `FusedLocationProviderClient` アダプター追加可）。初期 APK 軽量化のため ASR/TTS は OS 標準を基準とし、非対応端末は定型ボタンへフォールバック。 |
| **決定論コア層（共有）** | **狭い純粋 Rust コア**（`cargo-ndk` ＋ **薄い手書き C ABI / JNI**、arm64-v8a） | **CPU 完結・決定論・IO/DB/非同期/独自スレッドなし**。L1/L2 3 軸採点、Intent Contract / `PersonalRoutingOverride` 検証、`ManeuverHorizon` 正規化・グルーピング、P0〜P3 音声優先度スケジューラ、旅判定スコア、GPX 再生テスト。粗粒度 JSON 境界は **C ABI v1 の 12 関数**（[§2.1](#21-rust-c-abi-v1-と受け渡し方式)）に限定し、イベント入口は `dc_tick` 1 本（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。肥大時のみ `UniFFI` / Web `wasm-bindgen` へ切替検討。 |
| **経路エンジン層** | **Valhalla C++**（`Rallista/valhalla-mobile` 0.6.2 構成 NDK r29 / CMake / Ninja / vcpkg 起点 ＋ 有向辺取得用の薄い C++ 境界） | GraphHopper は Android オフライン公式サポート終了、OSRM は条件柔軟性・NDK 同梱に難があるため Valhalla を選定。既成 AAR は PBF 非対応であり、同一 Actor 呼出しから `Trip` と `Directions` の有向辺・index を抽出して JNI 返却する薄い C++ 境界を追加。**配送 ABI 全 `.so` の 16 KB ページ対応必須**。<br>**割り込みパッチ**: valhalla-mobile 0.6.2 に (1) route への割り込みコールバック (2) lock-free アトミックフラグによる `cancel(req_id)` の 2 点をパッチ（固定版ソースで割り込みコールバック対応を確認済み（sol6.2さん））。中断例外は **C++ ラッパー内で必ず捕捉して `cancelled` 終端に変換し、JNI を越えさせない**（探索キューと `search_finished` は [03 §3.1](03-routing-and-two-layer-control.md)）。<br>**属性 sidecar の読み取り**: タイル単位の固定長バイナリを mmap し、タイル内有向辺インデックスで引く。C++ は SQLite を使わない（[ADR-0002](../adr/0002-narrow-pure-rust-core.md) の二重リンク回避）。 |
| **ローカル SLM 推論層** | **別プロセス (`android:process=":inference"`, `exported=false`)** に隔離<br>・基準（Tier 1）: **`llama.cpp` C API**（CPU NEON）＋ **`Qwen2.5-0.5B-Instruct` Q4_K_M** (491 MB, Apache-2.0) ＋ **GBNF**（モデル本体は任意 DL）<br>・加速（Tier 2）: **`llama.cpp` Vulkan GPU** ／ 実証済み SoC 向け **`LiteRT-LM` NPU** 別アダプター | **Rust コアには一切繋がず**、Kotlin の推論コントローラ配下に限定。ドライバ障害やハングでナビ本体が落ちないよう別プロセス隔離。 |
| **Web 補完版** | **TypeScript ＋ Vite ＋ React ＋ Tailwind CSS ＋ Zod ＋ MapLibre GL JS ＋ `pmtiles`** → **Cloudflare Pages** | AI コーディングで最も安定するデファクト構成。「家で計画し、家で振り返る」専用で走行ナビは実装しない。共有スキーマは JSON Schema を正として Zod と Rust `serde` 型の双方を生成・検証。 |
| **全国データ生成 & 配布** | 固定 Docker (Linux) ＋ **`Planetiler`**（PMTiles）＋ **`Valhalla Mjolnir`**（全国一括）＋ **Python / Rust**（ABR・文化財・歴史 SQLite、属性 sidecar）→ **Cloudflare R2** | アプリと生成側 Valhalla のコミット・グラフ版を完全一致させ、同一版から地域/回廊チャンクを分割配布。<br>**属性 sidecar**: タイル単位の固定長バイナリ（mmap 前提、タイル内有向辺インデックス）とし、manifest で `graph_version`・SHA-256 と組にする。元タグ欠損 = `unknown`、sidecar 欠落/破損 = **データ不完全**。バイト配置の正本は `pipeline/packs/sidecar-format.md`（JSON Schema には複製しない）。Gate 2 の検証と C++ 読み取りテストで **同じバイナリフィクスチャ** を使う。Rust は sidecar を直接読まず、採点用 JSON（`schema/`）を受け取る。 |

---

## 2. モノレポ構成とモジュール境界の不変条件

空白を含むパスは NDK / CMake / vcpkg のビルドで壊れ得るため、リポジトリは **空白を含まないパスに clone することを推奨** します。

```text
<repo-root>/
├── schema/                   # JSON Schema（JSON の形の唯一の正）：common / trip-plan-snapshot / candidate-facts /
│   │                         #   maneuver-horizon / personal-routing-override / tick（dc_tick 入出力。意味は ADR-0008）
│   │                         #   ※ IntentContract・DriveDiary・exchange-v1 は未作成
│   ├── fixtures/             # Rust (serde) と Web (Zod) の共通適合テストデータ
│   └── slm/
│       ├── override.gbnf     # L3 1 行命令コードの GBNF 正本（docs の写しは 03 §4.1）
│       └── expected_codes.json # Gate 4 期待命令コード表の正本
├── core-rust/                # 狭い純粋 Rust コア（IO・DB・HTTP・スレッド・JNI 詳細禁止）
│   ├── Cargo.toml            # workspace
│   ├── crates/contracts/     # RouteKey, ManeuverHorizon, Evidence<T>, Intent DSL, 型検証
│   ├── crates/drive-core/    # L1/L2採点, CandidatePlanner, OverrideValidator, Horizon, 音声スケジューラ
│   ├── crates/native-bindings/ # 薄い C ABI / JNI 変換（panic 境界漏洩禁止）
│   └── tests/fixtures/       # 合成路網・固定 Valhalla Snapshot・GPX イベント列
├── android/
│   ├── app/                  # Kotlin + Jetpack Compose（画面 S01〜S16, NavigationSession, 3手先HMI）
│   ├── platform/             # LocationSource, TTS/ASR, FGS, ADPF Thermal, MapLibre, sqlite-bundled, R2 DL
│   ├── native/valhalla/      # ピン留め Valhalla C++, Trip/Directions 正規化, JNI
│   └── native/inference/     # :inference 別プロセス Service, llama.cpp C API, LiteRT-LM アダプター
├── web/
│   ├── src/                  # React + TS + Vite + Tailwind + Zod + MapLibre GL JS (W01〜W04)
│   └── tests/                # schema/fixtures との整合テスト
├── pipeline/
│   ├── sources/              # OSM PBF / デジタル庁 ABR / 文化庁・Wikipedia 取得・ライセンス管理
│   ├── graph/                # Valhalla Mjolnir ビルド設定・同版有向辺属性 sidecar 生成
│   ├── packs/                # Planetiler PMTiles, SQLite 構築, 署名付き manifest 生成
│   │   └── sidecar-format.md # 属性 sidecar バイナリ仕様の正本（バイト配置）
│   └── harness/              # Gate 3 ハーネス（OSM タグ検出率計測, 40 OD 監査, ndjson 記録）, GPX リプレイ評価, Gate 4 の 120 発話評価データ＆ハーネス
└── docs/                     # 設計図・仕様書・ADR（本ディレクトリ）
```

### 2.1 Rust C ABI v1 と受け渡し方式

`crates/native-bindings/` が公開する C ABI v1 は次の **12 関数** に固定します（イベント入口を `dc_tick` 1 本にする理由は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md)）。

| # | 関数 | 用途 |
|---:|---|---|
| 1 | `dc_abi_version` | ABI 版と schema 版を返す |
| 2 | `dc_buf_free` | Rust が確保した出力 `dc_out` を解放 |
| 3 | `dc_session_new` | 走行セッションを作成 |
| 4 | `dc_session_free` | 走行セッションを破棄 |
| 5 | `dc_plan_candidates(s, req)` | `req = {now_mono_ms, eta_estimate_s?, snapshot: TripPlanSnapshot}`。session の計画処理（`plan_request_id` と距離区分の固定、候補の待ち行列）を始め、探索要求の一覧と `planner_cmds` を返す（[03 §3.1](03-routing-and-two-layer-control.md)） |
| 6 | `dc_rank` | 候補の事実を L1/L2 で採点し、迂回上限で絞って比較表を作る（迂回上限は `dc_avoid_offer_eval` と同じ関数。[03 §3.2](03-routing-and-two-layer-control.md)） |
| 7 | `dc_validate_override` | 1 行命令コードを解析・検証・展開し `Applied` / `Rejected` を返す（下記・[03 §4.1・§4.2](03-routing-and-two-layer-control.md)） |
| 8 | `dc_trip_score` | ルート確定時に旅判定スコアを計算する（旅モード C01 を提示するかの判定。到着後の評価ではない。[05 §6.1](05-inference-tier-and-audio.md)） |
| 9 | `dc_route_commit(s, {req_id?, expected_route_key: RouteKey \| null, bundle})` | `req_id` は Rust が出した `commit{req_id}` と一致するときだけ受け付け、一致しなければ `E_STALE`（別の `commit` と取り違えないため。最初の経路だけは `req_id` 無しでよい）。確定経路（`route_bundle`、[04 §3.1](04-maneuver-horizon-and-hmi.md)）を読み込む。`expected_route_key` は今の経路の版で、一致しなければ `E_STALE`。`null` を許すのは今の経路がまだないとき（最初の経路）だけで、経路があるのに `null` なら `E_STALE`。`bundle.route_key.route_revision` は Kotlin が `"0"`（未確定）で渡し、それ以外なら `E_SCHEMA`。**版は Rust が付ける**（今の版 + 1、最初の経路は 1）。`bundle` 内で `route_key` を持つ場所（`ManeuverId` を含む）はすべて新しい版にそろえ、付けた `route_key` を結果として返す。`trip_id`・`trip_revision`・`graph_version` が今の経路と一致しなければ `E_STALE`（走行中は旧版にピン留めするため、旅の途中でグラフの版は変わらない。[06 §3](06-japan-data-packs-and-web.md)）。戻り値は付けた `route_key` と、読み込んだ legs・segments・maneuvers の件数。**旅の途中の再計画（経由地や目的地の変更で `trip_revision` が変わる場合）にこの入口からどう経路を入れるかは未決** |
| 10 | `dc_tick` | 時刻とイベントから Horizon・音声・画面・探索指示を出す唯一のイベント入口（[ADR-0008](../adr/0008-dc-tick-single-event-entry.md)） |
| 11 | `dc_avoid_offer_eval(s, {req_id, now_mono_ms, alt_facts, base_facts})` | 走行中の回避チップ判定（`dc_rank` と同じ迂回上限関数を使用）。`alt_facts`（代わりの経路）と `base_facts`（元の経路の探索開始位置から先）を同じ物差しで比べ（[04 §4.3](04-maneuver-horizon-and-hmi.md)）、迂回上限に収まれば `AvoidOffer` を作って Horizon に載せる。`offer_id` = `avoid-<req_id>`、`alt_route_id` = `req_id`、音声の `template_id` = `avoid_chip`（暫定）。**呼ぶきっかけは `planner_cmds: evaluate_offer{req_id, facts_ref}`**（`rank` と同じく、Rust が `CandidateFacts` を持たないため Kotlin に呼ばせる） |
| 12 | `dc_exchange_validate` | `exchange-v1` 受信データの検証（[06 §4.2](06-japan-data-packs-and-web.md)） |

*(用途の正本は C ヘッダ `dc_core.h` のコメント。本表はその要約です。)*

- **バッファ所有規約**: 入力 `dc_in{ptr,len}` は **呼び出し側所有**（関数復帰まで有効）。出力 `dc_out` は **Rust が確保** し、呼び出し側が `dc_buf_free` で解放します。
- **返り値形式**: `{ ok, data | error{ code, msg } }`。Rust 内の panic は `catch_unwind` で捕捉し `E_PANIC` として返します（panic を境界の外へ漏らさない）。
- **呼び出しスレッド**: 呼び出すのは **`NavigationSession` actor 1 本のみ**。Rust はセッション状態を持ちますが、**IO・スレッド・時計を持ちません**。
- **`dc_validate_override`**: 入力 `{ code_line, contract, utterance_id, utterance_mono_ms, ref_frame_seq, pending_offer_id? }`（`utterance_id` は必須。Gate 4 の遅延突き合わせ用） → 出力 `Applied { override_id, override, review_proposal?, route_key, expires_at_mono_ms } | Undone { req_id } | Rejected{ code }`（`Undone` は `CXL` で回避チップの経路変更を戻すと決めたとき。戻す `commit` の `req_id` を返す）。型名で、JSON では判別タグ `"result": "applied" | "undone" | "rejected"`（snake_case）として出します。1 行命令コードの解析は Rust で行い、Kotlin で JSON に組み直しません。
- **単調時刻規約**: Kotlin が毎回 `SystemClock.elapsedRealtimeNanos()/1e6` を `now_mono_ms` として渡します。Fix の時刻は `Location.getElapsedRealtimeNanos()` 由来。時刻の巻き戻りを検出した場合は前値に合わせ、`clock_regression` をログに記録します。GPX リプレイでは合成時計を使います。単調時刻（`MonoMs`）は JSON の **整数（number）** で渡します。十進文字列（`U64Str`）にするのは、OSM / Valhalla 由来の ID・revision など **Web に渡りうる `u64`** だけです（下記境界ルール 3）。
- **`dc_tick` の型**: `TickInput` / `Event` / `TickOutput` の型定義全文は [ADR-0008](../adr/0008-dc-tick-single-event-entry.md) を正本とし、ここには重複記載しません。

#### C++ → Kotlin → Rust の受け渡し
- C++（Valhalla ラッパー）が **採点入力 JSON を 1 回だけ生成** し、Kotlin は **`ByteArray` のまま** Rust へ渡します（再シリアライズしない）。
- **計測**: 計測点は JNI ラッパー側。短距離 7 候補で「受け渡し＋解析」の p95 が **300 ms**（暫定初期値。数値の正本は [07 §1](07-parameters-and-poc-gates.md)）を超えた場合は内訳を調査し、コピーが主因なら `DirectByteBuffer` 方式を比較します。

### 越えてはならない境界ルール
1. **Rust ↔ C++ 直接結合の禁止**: Kotlin 層が「Kotlin ↔ Rust」「Kotlin ↔ Valhalla C++」「Kotlin ↔ Inference (`:inference`)」を調停し、Rust と C++ を直接リンクしません（`libc++_shared.so`・シンボル衝突防止）。
2. **Rust コアの純粋性維持**: Rust 内に SQLite (`rusqlite`)・HTTP・ファイル I/O・Android ログを持ち込まず、Kotlin 側で取得した確定レコード（候補集合、属性、位置 Fix）のみを入力として純粋計算します。
3. **JSON の `u64` ID 表現**: TypeScript (IEEE 754 `number` は 53-bit 上限) での丸めを防ぐため、OSM / Valhalla 系の `u64` ID・revision など Web に渡りうる値は JSON 境界で **十進文字列**（`U64Str`）として扱います。単調時刻（`MonoMs`）など他の数値は JSON の整数です。
4. **16 KB ページサイズ必須検査**: `Valhalla`・`Rust cdylib`・`llama.cpp`・`MapLibre`・`sqlite-bundled`・`libc++_shared` を含む最終配送 ABI (`arm64-v8a`) の全 `.so` について ELF LOAD alignment ≥ 16 KB、`bundletool dump config` (`PAGE_ALIGNMENT_16K`)、`zipalign -c -P 16 -v 4` を検査ゲートにします。

---

## 3. 開発環境とリリースビルド環境の役割分担

ユーザーの手元環境および利用可能機材を踏まえ、役割を以下のように明確分離します。

### 3.1 日常開発・Android ビルド・地域 PoC（Mac M3 ＋ 外付け SSD）
- **実機スペック（実測値）**: Apple M3 (arm64, 8 コア) / RAM 16 GB / 内蔵 SSD 空き **21 GiB** / 外付け SSD `/Volumes/suneast`（**Case-sensitive APFS・空き 426 GiB**）。
- **原則**: 内蔵 SSD の空き容量（21 GiB）を圧迫しないため、SDK・NDK・Gradle・Cargo・Docker 仮想ディスク・ビルド出力は **すべて環境変数で `/Volumes/suneast` 配下へ固定** します。また、外付け SSD 未マウント時に内蔵 SSD へキャッシュを再生成して溢れさせないよう、ビルドスクリプト冒頭にマウントチェックを入れます。

```sh
# 推奨環境変数設定例（~/.zshrc 等。マウント確認とセットで運用）
export DEV_EXT="/Volumes/suneast/dev"
export ANDROID_HOME="$DEV_EXT/android-sdk"
export ANDROID_USER_HOME="$DEV_EXT/android-user"
export ANDROID_AVD_HOME="$DEV_EXT/android-avd"
export GRADLE_USER_HOME="$DEV_EXT/gradle"
export CARGO_HOME="$DEV_EXT/cargo"
export CARGO_TARGET_DIR="$DEV_EXT/cargo-target"
export RUSTUP_HOME="$DEV_EXT/rustup"
export SCCACHE_DIR="$DEV_EXT/sccache"
export npm_config_cache="$DEV_EXT/npm-cache"
```

- **16 GB RAM 環境の安定運用設定**:
  - `gradle.properties` のヒープ・並列度を制限（例：`org.gradle.jvmargs=-Xmx2g〜3g`、`org.gradle.workers.max=2〜4`）、CMake/Ninja 並列 `-j2〜-j4`、Cargo jobs `-j2〜-j4` から開始。
  - 重い C++ クロスビルド中はエミュレータや Docker VM を停止し、スワップ増大を防ぎます。
  - 外付け SSD のビルド用ディレクトリは Spotlight (`mdutil -i off`) および Time Machine の対象外に設定します。
  - Case-sensitive APFS のため、大文字小文字のみ異なるファイル名の作成を禁止します。

### 3.2 検証の役割分担（エミュレータ vs Android 実機 2 台）

*(注: Orca 組み込みエミュレータの存在・可用性は **未確認** です。使えるかどうかは **Gate 0 で確認** します（[07 §2](07-parameters-and-poc-gates.md)）。下表の「エミュレータ」行は、Gate 0 で確定した環境が担当します。)*

| 検証環境 | 担当する検証内容 | 検証不可（任せない）項目 |
|---|---|---|
| **Mac ローカル (`cargo test`)** | Rust 純粋コア、3 軸採点、Safety Gate、`ManeuverHorizon`、GPX リプレイ決定論テスト | 実機描画・C++ NDK ABI |
| **エミュレータ** (`arm64`、Orca エミュレータの可用性は Gate 0 で確認) | Kotlin UI、3 手先パネル表示、SQLite (`FTS5`/`R*Tree`)、Valhalla 経路計算・リルート、`LocationSource` リプレイ、16 KB イメージ起動スモーク | Tier 2 GPU/NPU 加速、実測 PSS・電池、ADPF 熱、実 TTS 消音遅延 |
| **手元の Android 実機（ハイエンド & ミドルレンジ）** | **Tier 1 / Tier 2 (GPU/NPU) 実測、60 分 GPX 持続テスト、ADPF Thermal 縮退、アプリ全体 PSS、TTS 割込 250ms、実走行（同乗者操作）** | — |

### 3.3 日本全国一括リリース生成（専用 Linux / Windows WSL2 環境）
- **前提**: 日本 PBF（2.574 GB）全体から `Planetiler` (PMTiles) ＋ `Valhalla Mjolnir` (L0/L1/L2 + 階層ショートカット + 同版有向辺属性) ＋ 全国 SQLite を一括生成する処理は、M3 16 GB や GitHub Actions 標準ランナー（SSD 14 GB 枠）ではメモリ・ディスク制約により保証できません。
- **運用方針**: ユーザーが用意する **専用 Linux マシンまたは Windows (WSL2) Docker 環境** をリリース生成ホストとして固定 Docker/OCI コンテナを実行し、生成した署名付きパックとマニフェストを直接 Cloudflare R2 へアップロードします。
  - **初期計画スペック**: Linux x86_64（またはネイティブ環境）、8 vCPU、**RAM 32 GB 以上、NVMe SSD 空き 150 GB 以上**、初回計算枠 6 時間（実測で RSS 24 GB または scratch 120 GB に迫る場合は 64 GB / 300 GB 級へ引き上げ）。
  - GitHub Actions 標準ランナーは、JSON Schema 適合テスト・小地域フィクスチャテスト・Android AAB の 16 KB アラインメント検証に限定します。
