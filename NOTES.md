# ISUCON14 チューニング作業メモ

## 環境 (isuenv, 2026-10-29 構築, TTL 10h)

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
- 計測: STATS=1 で 1 秒ごとに需給・区間時間をログ (journalctl -u isuride-go | grep stats)。

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
