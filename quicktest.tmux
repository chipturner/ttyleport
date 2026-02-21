# simple, quick test via an intermediate socket bridge
# usage: tmux -L ttyleport-test start-server\; source-file quicktest.tmux
new-session -d -s tty 'RUST_LOG=debug cargo run -- new-session --foreground'
split-window -v 'sleep 1 && cargo run -- attach -t 0'
attach-session -t tty
