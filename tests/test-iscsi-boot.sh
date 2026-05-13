#!/usr/bin/env bash
# iSCSI boot path end-to-end test
# Validates: nbdkit erofs plugin → nbd-client → tgtd → iSCSI → QEMU → UEFI → EROFS boot
set -euo pipefail

TEST_IMAGE="${BCVK_TEST_IMAGE:-quay.io/fedora/fedora-bootc:latest}"
NBD_PORT=10950
ISCSI_PORT=3260
SSH_PORT=2200
LOG_DIR="/tmp/bcvk-iscsi-test-logs"
SSH_KEY="/tmp/bcvk-iscsi-test-key"
EFI_CODE="/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
EFI_VARS="/tmp/bcvk-iscsi-efi-vars"
NBD_CONTAINER="bcvk-nbd-iscsi-test"
PLUGIN_PATH="/var/tmp/bcvk/libnbdkit_erofs_plugin.so"
IQN="iqn.2025-05.dev.bcvk:ephemeral"

MACHINE=""
QEMU_BIN=""
ROOTFUL=false
MERGED=""
QEMU_PID=""
TUNNEL_PID=""
PASS=0
FAIL=0

mkdir -p "$LOG_DIR"

cleanup() {
    echo ""
    echo "=== Cleanup ==="
    set +e
    [[ -n "$QEMU_PID" ]]  && kill "$QEMU_PID" 2>/dev/null && wait "$QEMU_PID" 2>/dev/null
    [[ -n "$TUNNEL_PID" ]] && kill "$TUNNEL_PID" 2>/dev/null
    if [[ -n "$MACHINE" ]]; then
        podman machine ssh "$MACHINE" -- "tgtadm --lld iscsi --op delete --force --mode target --tid 1" 2>/dev/null || true
        podman machine ssh "$MACHINE" -- "killall tgtd" 2>/dev/null || true
        podman machine ssh "$MACHINE" -- "nbd-client -d /dev/nbd0" 2>/dev/null || true
        podman machine ssh "$MACHINE" -- "podman rm -f $NBD_CONTAINER" 2>/dev/null || true
        if [[ -n "$MERGED" ]]; then
            podman machine ssh "$MACHINE" -- podman image umount "$TEST_IMAGE" 2>/dev/null || true
        fi
    fi
    rm -f "$SSH_KEY" "${SSH_KEY}.pub" "$EFI_VARS" 2>/dev/null
    echo "Done."
}
trap cleanup EXIT

check() {
    local label="$1" result="$2" expected="$3"
    if echo "$result" | grep -q "$expected"; then
        echo "  PASS  $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $label (expected: $expected, got: $result)"
        FAIL=$((FAIL + 1))
    fi
}

run_ssh() {
    ssh -p "$SSH_PORT" -i "$SSH_KEY" \
        -o IdentitiesOnly=yes \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout=2 \
        root@localhost "$@" 2>/dev/null
}

# ── Phase 0: Prerequisites ──────────────────────────────────────────

echo "=== Phase 0: Prerequisites ==="

echo -n "  QEMU (with libiscsi)... "
for candidate in \
    /tmp/qemu-11.0.0/build/qemu-system-aarch64-unsigned \
    /opt/homebrew/bin/qemu-system-aarch64 \
    "$(command -v qemu-system-aarch64 2>/dev/null || true)"; do
    [[ -n "$candidate" && -x "$candidate" ]] || continue
    if otool -L "$candidate" 2>/dev/null | grep -q libiscsi; then
        QEMU_BIN="$candidate"; break
    fi
done
[[ -n "$QEMU_BIN" ]] || { echo "MISSING (need QEMU built with libiscsi)"; exit 1; }
echo "OK ($QEMU_BIN)"

echo -n "  EFI firmware... "
[[ -f "$EFI_CODE" ]] || { echo "MISSING ($EFI_CODE)"; exit 1; }
echo "OK"

echo -n "  Podman machine... "
MACHINE=$(podman machine info --format '{{.Host.CurrentMachine}}' 2>/dev/null)
[[ -n "$MACHINE" ]] || { echo "MISSING"; exit 1; }
echo "OK ($MACHINE)"

echo -n "  rootful... "
if [[ "$(podman machine ssh "$MACHINE" -- id -u 2>/dev/null)" == "0" ]]; then
    ROOTFUL=true; echo "yes"
else
    ROOTFUL=false; echo "no"
fi

echo -n "  nbd module + /dev/nbd0... "
podman machine ssh "$MACHINE" -- "lsmod | grep -q '^nbd ' || modprobe nbd max_part=8" 2>/dev/null
podman machine ssh "$MACHINE" -- test -b /dev/nbd0 2>/dev/null || { echo "FAIL"; exit 1; }
echo "OK"

echo -n "  nbd-client... "
podman machine ssh "$MACHINE" -- which nbd-client &>/dev/null || { echo "MISSING (rpm-ostree install --apply-live nbd)"; exit 1; }
echo "OK"

echo -n "  tgtd... "
podman machine ssh "$MACHINE" -- which tgtd &>/dev/null || { echo "MISSING (rpm-ostree install --apply-live scsi-target-utils)"; exit 1; }
echo "OK"

echo -n "  erofs plugin... "
podman machine ssh "$MACHINE" -- test -f "$PLUGIN_PATH" 2>/dev/null || { echo "MISSING ($PLUGIN_PATH)"; exit 1; }
echo "OK"

echo -n "  SSH key... "
rm -f "$SSH_KEY" "${SSH_KEY}.pub"
ssh-keygen -t ed25519 -f "$SSH_KEY" -N "" -q
SSH_PUBKEY=$(cat "${SSH_KEY}.pub")
echo "OK"

# ── Phase 1: iSCSI target setup ─────────────────────────────────────
# nbdkit: コンテナ内 (bcvk と同じパターン)
# nbd-client, tgtd: ホスト上で直接実行 (rpm-ostree install --apply-live)

echo ""
echo "=== Phase 1: iSCSI target setup ==="

echo -n "  image... "
podman image exists "$TEST_IMAGE" 2>/dev/null || podman pull -q "$TEST_IMAGE" >/dev/null
echo "OK"

echo -n "  mount overlay... "
if $ROOTFUL; then
    MERGED=$(podman machine ssh "$MACHINE" -- podman image mount "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')
else
    MERGED=$(podman machine ssh "$MACHINE" -- podman unshare podman image mount "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')
fi
[[ -n "$MERGED" ]] || { echo "FAILED"; exit 1; }
echo "OK ($MERGED)"

CMDLINE="root=/dev/sda2 ro rootfstype=erofs console=ttyAMA0 console=tty0 loglevel=4 selinux=0 net.ifnames=0 systemd.journald.storage=volatile"

# 1. nbdkit (コンテナ内、bcvk と同じコマンド文字列パターンで SSH 経由クォーティング)
echo -n "  nbdkit... "
podman machine ssh "$MACHINE" -- "podman rm -f $NBD_CONTAINER" &>/dev/null || true

# ssh_pubkey はスペースを含むためダブルクォートエスケープで渡す
ESCAPED_PUBKEY=$(echo "$SSH_PUBKEY" | sed 's/"/\\"/g')

podman machine ssh "$MACHINE" -- \
    "podman run -d --name $NBD_CONTAINER --security-opt label=disable \
    -p ${NBD_PORT}:10809 \
    -v ${MERGED}:${MERGED}:ro \
    -v ${PLUGIN_PATH}:/plugin.so:ro \
    -v /usr/bin/nbdkit:/usr/bin/nbdkit:ro \
    -v /usr/lib64/nbdkit:/usr/lib64/nbdkit:ro \
    quay.io/fedora/fedora:latest \
    nbdkit -f -p 10809 -r /plugin.so \
    'dir=${MERGED}' \
    'cmdline=${CMDLINE}' \
    \"ssh_pubkey=${ESCAPED_PUBKEY}\"" 2>&1
echo ""

echo -n "  nbdkit ready... "
DEADLINE=$((SECONDS + 30))
NBD_READY=false
while [[ $SECONDS -lt $DEADLINE ]]; do
    # TCP 接続テスト — conmon ではなく実際の nbdkit に接続できるか
    if podman machine ssh "$MACHINE" -- "bash -c 'echo > /dev/tcp/localhost/${NBD_PORT}'" &>/dev/null; then
        NBD_READY=true; break
    fi
    sleep 1
done
if ! $NBD_READY; then
    echo "TIMEOUT"
    podman machine ssh "$MACHINE" -- "podman logs $NBD_CONTAINER" 2>&1 | tail -10
    exit 1
fi
echo "OK"

# 2. nbd-client (ホスト上で直接実行)
echo -n "  nbd-client → /dev/nbd0... "
podman machine ssh "$MACHINE" -- "nbd-client localhost $NBD_PORT /dev/nbd0 -readonly" 2>&1
NBD_SIZE=$(podman machine ssh "$MACHINE" -- blockdev --getsize64 /dev/nbd0 2>/dev/null | tr -d '\r\n')
echo "  (${NBD_SIZE} bytes)"

# 3. tgtd + iSCSI target (ホスト上で直接実行)
echo -n "  tgtd... "
podman machine ssh "$MACHINE" -- "killall tgtd 2>/dev/null; sleep 1; tgtd" 2>/dev/null
sleep 2
echo "OK"

echo -n "  iSCSI target... "
podman machine ssh "$MACHINE" -- "tgtadm --lld iscsi --op new --mode target --tid 1 -T $IQN" 2>/dev/null
podman machine ssh "$MACHINE" -- "tgtadm --lld iscsi --op new --mode logicalunit --tid 1 --lun 1 -b /dev/nbd0" 2>/dev/null
podman machine ssh "$MACHINE" -- "tgtadm --lld iscsi --op bind --mode target --tid 1 -I ALL" 2>/dev/null
echo "OK"

echo -n "  iSCSI verify... "
TGT_SHOW=$(podman machine ssh "$MACHINE" -- "tgtadm --lld iscsi --op show --mode target" 2>/dev/null)
echo "$TGT_SHOW" | grep -q "$IQN" || { echo "FAIL"; echo "$TGT_SHOW" | head -5; exit 1; }
echo "OK"
echo "$TGT_SHOW" | grep -E "Target|LUN|Size|Backing" | head -8 | sed 's/^/        /'

# ── Phase 2: QEMU iSCSI boot ────────────────────────────────────────

echo ""
echo "=== Phase 2: QEMU iSCSI boot ==="

echo -n "  SSH tunnel (3260)... "
podman machine ssh "$MACHINE" -L ${ISCSI_PORT}:localhost:${ISCSI_PORT} -N &>/dev/null &
TUNNEL_PID=$!
sleep 2
kill -0 "$TUNNEL_PID" 2>/dev/null || { echo "FAIL"; exit 1; }
echo "OK"

echo -n "  EFI vars... "
dd if=/dev/zero of="$EFI_VARS" bs=1m count=64 2>/dev/null
echo "OK"

echo -n "  QEMU start... "
"$QEMU_BIN" \
    -machine virt -accel hvf -cpu host \
    -m 4096 -smp 4 \
    -drive if=pflash,format=raw,readonly=on,file="$EFI_CODE" \
    -drive if=pflash,format=raw,file="$EFI_VARS" \
    -drive driver=iscsi,transport=tcp,portal=127.0.0.1:${ISCSI_PORT},target="$IQN",lun=1,if=none,id=disk0,initiator-name=iqn.2025-05.dev.bcvk:initiator \
    -device virtio-scsi-pci,id=scsi0 \
    -device scsi-hd,drive=disk0,bus=scsi0.0 \
    -netdev user,id=net0,hostfwd=tcp::${SSH_PORT}-:22 \
    -device virtio-net-pci,netdev=net0 \
    -device virtio-rng-pci \
    -serial file:"$LOG_DIR/qemu-serial.log" \
    -nographic -display none -monitor none \
    &>"$LOG_DIR/qemu-stdout.log" &
QEMU_PID=$!
sleep 3
if ! kill -0 "$QEMU_PID" 2>/dev/null; then
    echo "FAIL"
    cat "$LOG_DIR/qemu-stdout.log" 2>/dev/null | tail -20
    exit 1
fi
echo "OK (PID $QEMU_PID)"

echo -n "  SSH wait (up to 120s)... "
SSH_READY=false
DEADLINE=$((SECONDS + 120))
ATTEMPT=0
while [[ $SECONDS -lt $DEADLINE ]]; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "FAIL (QEMU died)"
        tail -30 "$LOG_DIR/qemu-serial.log" 2>/dev/null | sed 's/^/    /'
        exit 1
    fi
    if run_ssh true; then SSH_READY=true; break; fi
    sleep 3
    ((ATTEMPT++))
    # 進捗表示
    if (( ATTEMPT % 5 == 0 )); then
        echo -n "${ATTEMPT}.. "
    fi
done
if ! $SSH_READY; then
    echo "TIMEOUT (${SECONDS}s)"
    echo "  Serial log tail:"
    tail -30 "$LOG_DIR/qemu-serial.log" 2>/dev/null | sed 's/^/    /'
    exit 1
fi
echo "OK (${SECONDS}s)"

# ── Phase 3: Validation ─────────────────────────────────────────────

echo ""
echo "=== Phase 3: Validation ==="

check "kernel cmdline"     "$(run_ssh cat /proc/cmdline)"         "root=/dev/sda2"
check "rootfs type"        "$(run_ssh mount | grep 'on / ')"     "erofs"
check "root device"        "$(run_ssh findmnt -n -o SOURCE /)"   "sda2"
check "SCSI disk exists"   "$(run_ssh test -b /dev/sda && echo yes)" "yes"
check "var-ephemeral unit" "$(run_ssh systemctl is-active bcvk-var-ephemeral.service 2>/dev/null || echo inactive)" "active"
check "/var writable"      "$(run_ssh 'touch /var/tmp/iscsi-test && rm /var/tmp/iscsi-test && echo ok' 2>/dev/null)" "ok"
check "/etc writable"      "$(run_ssh 'touch /etc/iscsi-test && rm /etc/iscsi-test && echo ok' 2>/dev/null)" "ok"
check "SSH key injected"   "$(run_ssh 'test -s /root/.ssh/authorized_keys && echo yes')" "yes"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
echo "  Serial log: $LOG_DIR/qemu-serial.log"

[[ $FAIL -eq 0 ]] && exit 0 || exit 1
