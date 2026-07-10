#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
SCRIPT="$ROOT/scripts/sync-primary.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

make_repo() {
  local name=$1 repo="$TMP/$1" remote="$TMP/$1.git"
  git init --bare --quiet "$remote"
  git init --quiet -b main "$repo"
  git -C "$repo" config user.email test@example.invalid
  git -C "$repo" config user.name test
  mkdir -p "$repo/scripts"
  cp "$SCRIPT" "$repo/scripts/sync-primary.sh"
  printf '#!/usr/bin/env bash\necho iris\n' > "$repo/scripts/iris-dev.sh"
  chmod +x "$repo/scripts/iris-dev.sh"
  git -C "$repo" add scripts
  git -C "$repo" commit --quiet -m initial
  git -C "$repo" remote add origin "$remote"
  git -C "$repo" push --quiet -u origin main
  printf '%s\n' "$repo"
}

elf_repo=$(make_repo elf)
printf '\177ELFaccidental-binary\n' > "$elf_repo/scripts/iris-dev.sh"
(cd "$elf_repo" && bash scripts/sync-primary.sh >/dev/null)
test "$(head -n 1 "$elf_repo/scripts/iris-dev.sh")" = '#!/usr/bin/env bash'
test -z "$(git -C "$elf_repo" status --porcelain=v1)"

text_repo=$(make_repo text)
printf '# legitimate edit\n' >> "$text_repo/scripts/iris-dev.sh"
set +e
(cd "$text_repo" && bash scripts/sync-primary.sh >/dev/null 2>&1)
code=$?
set -e
test "$code" -eq 10
grep -q 'legitimate edit' "$text_repo/scripts/iris-dev.sh"

printf 'sync-primary-tests: PASS\n'
