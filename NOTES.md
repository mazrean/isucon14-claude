# ISUCON14 チューニング作業メモ

## 環境 (isuenv, 2026-10-29 構築, TTL 10h)

| node | public | private | 役割 |
| --- | --- | --- | --- |
| isucon14-1 | 13.231.220.166 | 10.100.0.218 | nginx + app (ベンチ対象) |
| isucon14-2 | 54.95.92.62 | 10.100.0.176 | MySQL (永続化先) |
| isucon14-3 | 18.179.35.42 | 10.100.0.19 | 未使用 |
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
