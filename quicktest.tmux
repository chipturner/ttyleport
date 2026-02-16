# simple, quick test via an intermediate socket bridge: tmux start-server\; source-file quicktest.tmux
new-session -d -s tty 'RUST_LOG=debug cargo run -- new-session -t /tmp/server.sock --foreground'
split-window -v 'socat UNIX-LISTEN:/tmp/client.sock,fork UNIX-CONNECT:/tmp/server.sock,retry'
split-window -v 'cargo run -- attach -t /tmp/client.sock'
attach-session -t tty
