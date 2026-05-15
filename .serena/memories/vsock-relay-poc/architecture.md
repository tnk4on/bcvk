# vsock relay PoC アーキテクチャ

## vfkit vsock モード
- `connect` モード: vfkit が Unix socket を作成 → ホストプログラムが接続 → vfkit がゲストの vsock ポートに connect
- `listen` モード: ゲストが vsock で connect → vfkit がホストの既存 Unix socket に dial (vfkit はクライアント)

## SSH 接続
- vsock `connect` モード + ゲスト socat `VSOCK-LISTEN:1031,fork TCP:localhost:22`
- ホスト: `ssh -o ProxyCommand="socat - UNIX-CONNECT:/tmp/vsock-ssh-a.sock" fedora@localhost`
- `-o IdentitiesOnly=yes` 必須 (Too many auth failures 防止)

## リレー
- Rust バイナリが `--listen-a` と `--listen-b` で 2 つの Unix socket を作成
- 各 VM の vfkit は `listen` モードでリレーのソケットに接続
- ゲスト内で `socat VSOCK-CONNECT:2:1030 TCP:localhost:<port>` でブリッジ
