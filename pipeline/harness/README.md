# Gate 2・3・5 ホスト検証

統合設計 v1.2「G. 配布」「属性 sidecar」「C++ → Kotlin → Rust」「PSS」「Gate 3」に対応する。合成データで動作と拒否条件を検査し、実測値の採取・評価にも同じ評価器を使う。各 CLI の `criteria_met` は入力に対する対象項目の成立だけを示す。全国ビルド、実車案内、Android JNI、実機の持続試験は別途実行する。

## 再現

Python 3.11 以上、OpenSSL CLI。受け渡し試験のみ JDK（21 推奨）、Maven、C++17、Rust/Cargo（初回のみ Maven/Cargo の依存取得）。Python の追加依存は共有 JSON Schema を検証する場合のみ。以下はリポジトリルートから実行する。

```sh
python3 -m unittest discover -s pipeline/harness -v
work=$(mktemp -d)
python3 pipeline/harness/sample.py "$work/fixture"
python3 pipeline/harness/gate2.py "$work/fixture/gate2" "$work/fixture/gate2/test-public.pem"
python3 pipeline/harness/gate3.py "$work/fixture/gate3"
python3 pipeline/harness/gate5.py "$work/fixture/gate5"
python3 pipeline/harness/verify_handoff.py

# 共有 schema/ 取り込み後（別ツリーを読む場合は --schema-dir にそのパスを指定）
python3 -m venv "$work/env"
"$work/env/bin/pip" install 'jsonschema>=4.18,<5'
"$work/env/bin/python" pipeline/harness/verify_handoff.py --schema-dir schema
```

Gate CLI の終了値は成立 0、条件未達 1、入力不正 2。生成例は新規ディレクトリへのみ出力する。フィクスチャの公開鍵はその場で作ったテスト署名専用。本番の信頼鍵は入力ディレクトリから独立させて固定する。

## 有向辺の受け渡し

1. `handoff/replay.cpp` が固定した1探索の Trip/Directions に相当する、同じ leg・辺・操作位置から短距離7候補を生成する。再探索や shape への再マッチングは行わない。実側の接続点は、同一 Actor 呼出しで取得した Trip の各 leg/node/edge と Directions の操作位置である。実際の Actor とグラフのアクセス制限・タイル完全性判定は Gate 0・1 の入力になる。
2. 検体は `schema_version=1, plan_request_id, graph_version, candidates[]` を UTF-8 で1回生成する。方向は OSM way の順／逆、区間は**有向辺の走行方向**に対する [start_ppm,end_ppm]（0〜1,000,000）。leg 順・辺順、十進文字列 u64 の GraphId、辺の全長 cm、両方向通行・生活道路、幅の根拠、操作の leg/incoming index を保持する。幅・車線・方向不明は根拠種別で区別し、欠測をゼロ幅に変換しない。根拠の形式は `confirmed{value,source_ref}`、狭路推定は `inferred{value:bool,rule_id}`、`unknown{reason}`。版不一致を含む不完全候補は拒否する。検体が採点する操作は対向横断右折（信号未確認）で、未対応の操作を入力すると失敗する。
3. `Pass.kt` は固定要求 ID・版を照合し、受領した同じ `ByteArray` を Rust に渡す。ホストでは標準入出力で JNI を代替する。末尾 LF はパイプのフレーム境界である。SHA-256 で通過後の空白・キー順・日本語を含む生バイトが一致することも検証する。
4. Rust 検証器が部分辺距離、確認済み／推定狭路、幅未確認生活道路、右折根拠、方向付き指紋を集計する。合成の先頭候補は ETA 240,000 ms、確認済み狭路150 m、幅未確認生活道路150 m、信号未確認右折1回、採点352,500 ms。所要時間表示にはペナルティを加えない。`--schema-dir` では7件の集計出力を共有 `candidate-facts.schema.json` で検証する。
5. 50反復のホスト p95 には JVM 境界検査、プロセス間コピー、Rust 解析・集計、パイプの往復を含む。実機では JNI ラッパー入口と復帰を単調時計で計測し、7候補の受け渡し＋解析の p95 が300 msを超えたら工程別に計測する。コピーが主因の場合に DirectByteBuffer と比較する。

**統合境界**　この入力は生の有向辺を保持するテスト用キャプチャ。共有 `CandidateFacts` は集計後の型で、確認した `dc_core.h` の `dc_rank` は `{facts[],policy,profile}` を取り、実装は `E_UNIMPLEMENTED`。共有 ABI を追加せず、生の辺配列を受ける正式な採点入力と Rust 集計の接続箇所を統合担当と確定する。Kotlin の再直列化を必要とする変換は入れない。検体の C++ mmap 読み取りと Python verifier は同じ [sidecar 形式](../packs/sidecar-format.md) と同じ60バイトの検体を使う。

## Gate 2　連続全国2版

専用 Linux で固定 Japan PBF をスナップショットごとに一括処理する。PBF SHA-256、Valhalla 固定コミット、build_id、未分割グラフの全 tile base ID と各有向辺数、Planetiler・SQLite 等の出力、ビルドログ・環境・リソース実測を保存する。元グラフから採取したタイル一覧とマニフェストを照合する。署名が正しくても元の生成記録が全国をカバーしたことは発行者の証跡で確認する。

- `old.json` と `new.json` に `graph_version, previous_graph_version, build_id, build_scope, pbf_sha256, valhalla_commit, builder_tile_inventory, chunks, prefectures, regions, model, additional_r2_objects` を持たせる。両方に対する独立署名を `.json.sig` に置く。PoC の署名方式は OpenSSL の SHA-256 detached signature。検証済みマニフェストの物理キーを `manifests/<版>/manifest.json[.sig]`、チャンクキーを SHA-256 として扱う。正式マニフェストへの取り込みは同じ公開鍵と署名済みバイトを用いるアダプターで行う。
- グラフチャンクの PoC コンテナは ZIP。各タイルの `<tile>.gph` と `<tile>.dsc` を同梱し、署名された個別ハッシュ・タイルキー・sidecar の版・グラフ由来 edge_count を検証する。L0/L1 は全国共通、L2 の `cell=[南端緯度,西端経度]` は度単位の符号付き整数。セルは Valhalla L2 の最大4×4枠。空枠は不要。圧縮後256 MB以下となるよう生成側が固定したタイル境界で分割し、セル内の `part`、タイル重複、セル帰属を照合する。単一タイル自体が上限を超える場合は当該版を見送る。ビット割当と0.25度格子の出典は [Valhalla GraphId](https://github.com/valhalla/valhalla/blob/master/valhalla/baldr/graphid.h) と [TileHierarchy](https://github.com/valhalla/valhalla/blob/master/src/baldr/tilehierarchy.cc)。
- 全国概要、詳細地図、style（glyph/sprite を含む）、幹線、詳細グラフ＋属性、住所、歴史を列挙する。47県は取得索引、8地方は県索引の集合。 `run.json.initial_region` は実測する1地方。標準配置は新版全実体 ≤7.5 GB、連続2版の実体和集合 ≤9.0 GB、1地域初回 ≤750 MB、各配信チャンク ≤256 MB（十進）。同じ R2 にある SLM・マニフェスト・署名・メタデータ等も対象。追加実体は `additional_r2_objects` に実際の object_key・sha256・bytes・kind を全列挙する。同じキー・同じ内容だけを共有する。同一 R2 のモデルは `kind=model` の並び順で連結 SHA を検査する。外部固定版は revision・全体 SHA を記録する。
- 端末の `device_free_bytes` は旧版を保持した状態の空き容量。新規非共有の展開サイズ＋地域の最大配信チャンク＋`reserve_bytes` を要求する。非グラフの検体は直接配置するファイル。別の圧縮形式を追加する場合は非圧縮長と展開ファイル SHA のアダプターが必要。
- `run.json` の `od_plan` に固定した12 OD の ID・種別を、`od_runs` に県境4・地方跨ぎ3・離島フェリー2・都市山間3の一意な OD、取得量・リクエスト数・断片取得と機内モード再計算結果を記録する。端末・配布側のログから実測し、リトライも含める。回廊内外各10件の実測は `reroutes` に保存する。ホストは全検証後のみ一時領域の pointer を原子的に切り替え、走行開始時の版を保持する。署名不一致・破損・予算超過・空き不足・走行検証不足で現行版を維持する。実 R2 操作、再開ダウンロード、実機のクラッシュ時永続性は別途検証する。

## Gate 3　固定 PBF と40 OD の盲検監査

`sample.py` が全列を持つ入力を生成する。本番では以下の独立した証跡で置き換える。

| 入力 | 採取単位・内容 |
|---|---|
| `survey.json` | 固定 PBF SHA、graph_version、都道府県×道路種別ごとの独立した母集団件数（junction/section/approach）、検証済み全国マニフェストの tile・sidecar SHA・辺数。47県を含める |
| `tags.ndjson` | 固定日本 PBF を走査し、信号は車道交差点ノード、width/lanes は道路区間と長さ cm、turn:lanes は流入方向。タグのない対象も出力。県・道路種別・unit・unit_id でソートし各単位を1回だけ数える。境界での帰属・複数道路種別の分類規則を生成ログに固定する |
| `sidecars/<sha256>` | グラフ生成時の対応表から各 unit の対象有向辺 `edges=[{tile,index}]` を列挙し、実ファイルを読取専用 mmap する。行の `source` は独立に取得したタグ値で、確認済み幅は cm 整数の十進文字列、lanes は車線数の十進文字列。信号／方向対応した turn:lanes は存在時 `present`、欠損は null。v1 では turn:lanes の存在到達率を測る。元のレーン文字列や信号現示の内容一致を判定するには形式拡張が必要 |
| `cases.ndjson` | 事前固定した40 OD。都市部対向横断右折12、郊外狭路12、目的地入口方向8、山間／県境／離島8。地点・タイムゾーン付き出発時刻・普通車条件・版を固定 |
| `routes.ndjson` | 各 OD の result を1行（ok/no_route/incomplete_data/timeout/error）。candidate は別行で、同版の route_id・観察可能な route_ref・方向付き指紋・標準／採用・完全性・入口表示を記録 |
| `audits.ndjson` | 標準／採用を隠した route_ref を監査者に提示し、OD/route ID で結合する。合法性、対向横断右折数、狭路 cm、入口方向、出典と未確認箇所、監査者を記録。判定が割れた候補は独立した二人目と解決結果を残す |

到達率の分子は同じ属性が**対応する全有向辺**に届いた元タグ単位数、分母は元タグありの単位数。検出率・到達率とも幅／車線数は距離加重も併記する。変換不能な元タグを既知値にせず、元タグあり・到達せずとして記録する。母数0の率は null。v1.2 は到達率の最低値を定めていないため、この数値を校正資料にする。sidecar 欠損・破損・版混入は入力不正。

全40件の結果行を照合し、未実行・候補欠落・監査漏れ・合法性不明・数値未確認の OD を `pending` に残す。改善率の分母は、合法・完全で異なる指紋を持つ代替が確認できた集合の**標準候補の合計**。同じ OD 集合の採用候補と対で比較する。代替ありでも標準を採用した OD は改善0で含める。合法性違反・未確認入口の左側断定は代替なしを含む全候補で観測し、未確認分と分けて報告する。母数0、留保、違反、右折30%減／狭路20%減の未達では合格扱いしない。

## Gate 5　60分の PSS

1. ミドル機／ハイエンド機の物理実機を用意し、各機で充電あり・なしの60分 GPX を再生する。地図表示、1〜2分ごとの音声、再探索とモデルロードなどのイベントを記録する。ADB で端末を明示し、`adb -s SERIAL shell dumpsys meminfo -a --package PACKAGE` の**一つの出力**に対象本体と `:inference` の両プロセスの App Summary / TOTAL PSS が含まれることを Gate 0 で確認する。対象 OS で形式が違えば検証失敗になる。
2. 採取計画は `{"package":"jp.example.nav","charging":"off","events":[{"second":112,"event":"inference_reload"}]}` の形。GPX の開始を単調時計の0秒に合わせ、各イベント秒を登録する。定常は0〜3600秒の5秒間隔721点、イベント秒の前後1秒と当秒も採取する。採取なし／5秒／イベント前後1秒の計測負荷を Gate 0 で比較する。
3. 下のコマンドで新規ディレクトリに採取する。各 `<second>.txt` が1回の生出力、`samples.ndjson` が開始・終了・エラー記録。イベント実績の単調時計と秒を照合し、予定外イベントも追記する。必要な前後サンプルが無ければ欠測になる。60分中にプロセスが落ちた記録を logcat 等で確認し、`run.json.abnormal_exits` に列挙する（確認後にだけ空配列にできる）。中断した試験、PID 変化、片方のプロセス欠落、採取異常は不成立。
4. 本体と推論の TOTAL PSS を各観測で足して kB × 1024 bytes に換算する。定常点だけの nearest-rank p95 ≤1,300,000,000 bytes、イベント採取を含む全点の最大 ≤1,500,000,000 bytes を評価する。計測窓は名目時刻の−0.5〜＋1.0秒内（ハーネスの暫定許容値）。同一 dumpsys 呼出し内でも OS の内部走査に時間差があるため、終了時刻と開始時刻を保存する。
5. 4走行の集約で異なる2端末×充電有無を照合する。JankStats、SEVERE 累計、ANR/LMK/クラッシュ、熱・低メモリ時の停止／モデル解放時間、強制終了後の本体ナビ継続／降格は Gate 5 全体の別証跡として残す。

```sh
python3 pipeline/harness/collect_pss.py SERIAL plan.json new-run
python3 pipeline/harness/gate5.py new-run
python3 pipeline/harness/gate5.py mid-on mid-off high-on high-off
```

## 取り込み

隔離ワークツリーの `pipeline/harness/` と `pipeline/packs/sidecar-format.md` が対象。生成済み `handoff/target/` と `__pycache__/` は除外する。共有 `schema/`・`core-rust/`・JNI・`docs/specs/` は各担当の変更を使用する。統合後の CI では上記 unit test と `verify_handoff.py --schema-dir schema` を実行し、Android JNI の実測・全国2版・40 OD 人手監査・実機4走行の記録をリリース判断時に別途照合する。

## ホスト確認結果（2026-11-24）

Gate の単体検証23件が成功した。7候補のホスト受け渡しは50回、p95 1.38 ms。共有スキーマで全7件の Rust 集計出力を検証し、要求 ID・版不一致、必須辺属性欠落など7負例を拒否した。合成の Gate 2 は新旧で33 bytesを再利用し、現在版 v2 への切替後も実行中の参照は v1 だった。Gate 3 は40件のうち代替あり36件を分母に右折30%減・狭路20%減。Gate 5 は定常721点・全724点で p95 1,126,400,000 bytes、最大1,157,120,000 bytes。全国2版・40 OD の人手監査・Android JNI・実機4走行のリリース判定は未測定。
