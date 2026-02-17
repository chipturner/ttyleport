# simple, quick test via an intermediate socket bridge
# usage: tmux -L ttyleport-test start-server\; source-file quicktest.tmux
new-session -d -s tty 'RUST_LOG=debug cargo run -- new-session -t /tmp/ttyleport-quicktest.sock --foreground'
split-window -v 'socat UNIX-LISTEN:/tmp/ttyleport-quicktest-client.sock,fork UNIX-CONNECT:/tmp/ttyleport-quicktest.sock,retry'
split-window -v 'cargo run -- attach -t /tmp/ttyleport-quicktest-client.sock'
attach-session -t tty
