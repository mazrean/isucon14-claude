# ISUCON14 チューニング作業メモ

## 環境 (isuenv, 2026-10-29 構築, TTL 10h → 2026-10-30 08:33 UTC まで延長)

- TTL 延長: 各ノードの `/var/lib/isuenv-expires-at` (UNIX 時刻) と EC2 タグ `isuenv:expires-at` を書き換える。
- ベンチノードのメモリ制限 (mem=9G) はユーザー許可のもと解除済み (16GB, 8 vCPU のまま)。
  `/etc/default/grub.d/zz-isuenv-mem.cfg` を `/root/zz-isuenv-mem.cfg.disabled` に退避して update-grub。
- ベンチノードの CPU もユーザー許可のもと c5.2xlarge → c5.4xlarge → c7a.4xlarge (16 物理コア) に変更。
  vCPU クォータが 32 (競技 3 台で 6 使用) なので 26 vCPU 超は不可。停止/起動でパブリック IP が変わるので
  ~/.ssh/isuenv_config の HostName を書き換える (private IP 10.100.0.67 は不変)。

| node | public | private | 役割 |
| --- | --- | --- | --- |
| isucon14-1 | 13.231.220.166 | 10.100.0.218 | ベンチ対象。nginx + :443 新規接続を DNAT で 3 分割 (isucon-tls-lb.service) |
| isucon14-2 | 54.95.92.62 | 10.100.0.176 | app (オンメモリ Go, UDS /run/isuride/app.sock + :8080) + nginx 1 worker (UDS) |
| isucon14-3 | 18.179.35.42 | 10.100.0.19 | MySQL (write-behind 永続化先) + nginx |
| isucon14-bench | 54.248.56.216 | 10.100.0.67 | ベンチマーカー |

```sh
isuenv up isucon14 --nodes 3 --bench --ttl 10h   # 再構築
make all                                          # deploy + bench (Makefile 参照)
```

ベンチ: `ssh isucon14-bench 'cd /home/isucon && sudo -u isucon ./bench run --addr 10.100.0.218:443 --target https://isuride.xiv.isucon.net --payment-url http://10.100.0.67:12346 --payment-bind-port 12346'`

注意: ローカルの `cp` は `cp -i` エイリアス。スクリプトでは `command cp` を使う。

## スコア履歴

| commit | 内容 | score |
| --- | --- | --- |
| 0e87797 | 初期実装 | 1085 |
| 018b339 | MySQL 別ノード + nginx keepalive/http2 | 1796 |
| b0bb00c | オンメモリ化 + SSE + アプリ内マッチング + 決済 Idempotency-Key | 288237 |
| c3e519e | nginx / app / MySQL を 3 台に分割 | 303986, 308309, 325960 |
| (多数) | matching: 近い椅子を最大 3s 待つ / 空 nearby 遅延 300ms | 367995, 365092 |
| 8f…/be8f9a8 | /images を no-store (ベンチのメモリ 7.5G→4.7G) + 空 nearby 遅延 1s→2s | 407726, 406620 → 431149, 441564 |
| 79a0185 | 空 nearby をロングポーリング化し、空き椅子ごとに K 人だけ起こす | 642665, 641665 |
| fd8a885 | TLS 終端を DNAT で 2 ノードに分散 + components バンドル no-store | 645607, 695793 |
| 5b178da | 空き椅子 1 台あたり起こす人数 K=8 | 737836, 730988 |
| 89d95c0 | 同居 nginx→app を UDS 化、TLS 3 分割 | 727118, 733128, (全台再起動後) 750315 |
| 7f6bf32 | index.html/favicon を 1d キャッシュ | 774165, 741778 |
| 31f8a60 | 空 nearby の保留 8s | 865140, 870314 |
| 8bf650e | 空き椅子 1 台あたり K=32 人起こす | 938704, 947899 (※帯域バースト込み) |
| bb4d596 | no-store の静的ファイルも gzip (node1 送信帯域上限対策) | 788742, 803900, 789131, 757578 |
| 5e0139c | ベンチのメモリ制限解除 → 静的ファイルを全てキャッシュ可 | 1042661, 1046277 |
| (c5.4xlarge) | ベンチ 16 vCPU / nginx を app ノードから外し TLS 2 分割 | 1551290, 1555024 |
| (app) | マッチング・nearby 配布のロック短縮、空き椅子集合、GOGC=400 | 1625423, 1665539 |
| (c7a.4xlarge) | ベンチを c7a.4xlarge に | 1670344, 1758711 |
| (persist) | 座標を履歴でなく椅子ごとの集計で保存 (MySQL 負荷減) | 1783113, 1788217 |
| (rust) | webapp/rust: Go 版を Rust (tokio + hyper 1, 単一 parking_lot Mutex) に移植。同一ロック内 A/B で app CPU idle Go 4-10% → Rust 39-43% (スコアはベンチ律速で同等) | Rust 1919691, 1905784 / Go 1869299, 1925843 |

※ 90 万台は c5.large の帯域バーストクレジットに乗っていた。連続走行でクレジットが尽きると
  node1 の ENA bw_out_allowance_exceeded が増え (500 万パケット破棄)、再送・タイムアウトで崩れる。
  最終構成は ~3.6GB/走行 (~400Mbps 平均) でベースライン内、破棄 0。

## 知見

- ベンチノード (c5.2xlarge, 8 vCPU) が負荷走行中ほぼ CPU 100%。椅子の実移動時間の不満 (~55%) は
  主にベンチ側の tick 落ち。サーバー側は CPU に余裕あり。
- 静的ファイルに Cache-Control (max-age) を付けると大きく悪化 (239k, 197k)。付けない。
- 再起動試験: 負荷走行後に app/DB ノードを reboot → 行数一致・アプリ復帰を確認済み。
- 同時に動かす環境は 1 つだけ (ユーザー指示)。
- シナリオ: 供給律速 (椅子はほぼ常に満杯)。スコア ≈ 椅子が運んだ距離の総和、椅子は売上に比例して増える
  (正帰還)。ユーザーは招待で指数的に増えて余る。空き椅子が無いとユーザーは毎 tick 履歴確認 + nearby を
  繰り返し、ベンチの CPU を空回りで消費 → 椅子の tick が落ちる → スコア低下。
- ベンチのメモリ (8G) も天井。isucandar はユーザーごとに静的ファイルのキャッシュ本体を持つ。
  固まった走行が 2 回 (メモリ枯渇)。ユーザー判断: 「リスク許容で最大化」。
- owner の椅子追加判定は tick%60==59 ちょうどのみ。ベンチの CPU 飽和で約 3 割取りこぼす。
- 決済: 失敗率 min(50%, 直近3s件数)%、トークンごと 4 回失敗後は必ず成功。並列リトライは
  初回リクエストとの競合で二重決済の経路があるので不採用。
- HTTP/1.1 は h2 より悪い (255k, 271k)。gzip_static off は差なし。
- 再起動試験 (89d95c0): 全 3 台同時 reboot → DNAT/nginx/app/MySQL 自動復帰、GET /api/app/rides の応答 md5 と行数一致。
- UDS: app と同居する nginx のみ (別ホスト間は TCP)。同居ノードは CPU を取り合うので nginx は 1 worker。
- ベンチのプロファイル: 配布バイナリは strip 済み。同ソース + 埋め込みハッシュ (配布バイナリから抽出) で
  分析用コピーを作り取得 (採点には使わない)。静的取得+md5 31%、履歴確認 14%、決済サーバ 7%、GC ~20%。
- 不採用 (測定済み): 得点レート型マッチング (732k/775k)、迎車条件内レート優先 (911k/910k)、
  決済並列リトライ (差なし)、nearby 保留 9.3s / 猶予 250ms / http2_chunk_size (差なし)、
  GET /rides 保留 (評価↑→ユーザー増→悪化 844k)、/assets を 304 方式 (887k/866k)、全静的 no-store (614k)。
- 攻めの選択肢: components バンドルもキャッシュ可にすると ~915k だがベンチメモリ 7.0G/7.9G。
  town.svg までキャッシュすると 7.8G でベンチが固まった。固まると 0 点なので不採用。
- 計測: STATS=1 で 1 秒ごとに需給・区間時間をログ (journalctl -u isuride-go | grep stats)。

## Rust 版 (webapp/rust)

- `make deploy-rust` でビルド + デプロイ (既存の isuride-go.service の ExecStart のバイナリを置き換えるだけ)。Go に戻すのは `make deploy`。
- toolchain は rust-toolchain.toml の nightly (検証時 rustc 1.101.0-nightly 92ffebece 2026-10-28)、target-cpu=skylake-avx512、
  glibc 要求は 2.39 まで (Ubuntu 24.04 と同じ)。release は lto=fat / codegen-units=1 / panic=abort。
- WAL 形式は Go とバイト互換 (cargo test go_compat に Go 側出力を GO_WAL=... で渡して比較)。Go が書いた WAL を Rust が、その逆も replay できる。
- 外部動作は Go と同じ (GET 系の応答・エラー応答を同じ DB 状態で突き合わせて差分なし。ヘッダ名の大小文字と順序のみ異なる)。
  /api/initialize の language だけ "rust"。PPROF は Go のみ。RUST_WORKERS で tokio のワーカー数 (既定 = CPU 数)。

## 方針

1. Go 単一プロセスで全データをオンメモリ化。ミューテーションは行スナップショットの upsert として
   キューに積み、1 goroutine が MySQL (node2) へバッチ書き込み (write-behind)。
   起動時・`/api/initialize` 時は MySQL から全ロード。SIGTERM でキューを flush してから終了
   (systemd: KillSignal=SIGTERM, TimeoutStopSec=60) → 再起動後もデータが残る。
2. マッチングをアプリ内 goroutine に移す (isuride-matcher は停止)。距離/速度ベースの貪欲割当。
3. 通知エンドポイントを SSE 化 (ベンチ側対応を確認中)。
4. 決済: Idempotency-Key 付きでリトライ (ベンチ側対応を確認中)。
5. インフラ: nginx keepalive / ログ off / sysctl / MySQL 別ノード。

## 進行中

- インフラ (Makefile, nginx, sysctl, MySQL 移設): サブエージェント担当
- ベンチ解析 (スコア式・SSE 形式・決済仕様): サブエージェント担当 → scratchpad/bench-analysis.md
- オンメモリ本体の書き換え: メインセッション
