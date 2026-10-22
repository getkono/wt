#!/usr/bin/env bash
# Regenerate the README screenshot (assets/wt.svg) from a real `wt` frame: run the
# TUI in a detached tmux pane against a throwaway demo repo, capture the pane as
# ANSI, and render it with freeze. Needs tmux; `mise install` provides freeze.
set -euo pipefail
cargo build --release && wt="$PWD/target/release/wt" out="$PWD/assets/wt.svg"
s=/tmp/wt-demo; rm -rf "$s"; mkdir "$s"; trap 'tmux -L wt-svg kill-server 2>/dev/null; rm -rf "$s"' EXIT
export HOME="$s" GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=demo GIT_AUTHOR_EMAIL=demo@wt.invalid GIT_COMMITTER_NAME=demo GIT_COMMITTER_EMAIL=demo@wt.invalid
export GIT_AUTHOR_DATE="$(date -d '-3 hours' -R)" GIT_COMMITTER_DATE="$(date -d '-3 hours' -R)"
mkdir "$s/acme" && cd "$s/acme"
git init -q -b main && echo '# acme' > README.md && git add . && git commit -qm 'feat: initial api server'
for b in feat/login fix/session-timeout docs/api; do "$wt" -y new "$b" >/dev/null; done
git -C ../acme.worktrees/acme-feat-login commit -q --allow-empty -m 'feat(auth): add OAuth login flow'
echo x >> ../acme.worktrees/acme-fix-session-timeout/README.md
git branch chore/bump-deps
tmux -L wt-svg -f /dev/null new-session -d -x 140 -y 12 -e HOME="$HOME" -e TERM=xterm-256color "$wt"
sleep 1; tmux -L wt-svg send-keys j j; sleep 1
tmux -L wt-svg capture-pane -e -p | freeze --language ansi -o "$out"
