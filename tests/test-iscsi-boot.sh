#!/usr/bin/env bash
# iSCSI boot path end-to-end test
# Validates: nbdkit erofs plugin → nbd-client → tgtd → iSCSI → QEMU → UEFI → EROFS boot
set -euo pipefail

TEST_IMAGE="${BCVK_TEST_IMAGE:-quay.io/fedora/fedora-bootc:latest}"
ISCSI_PORT=3260
SSH_PORT=2200
VM_NAME="iscsi-test"
LOG_DIR="/tmp/bcvk-iscsi-test-logs"
SSH_KEY="/tmp/bcvk-iscsi-test-key"
EFI_CODE="/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
EFI_VARS="/tmp/bcvk-iscsi-efi-vars"
NBD_CONTAINER="bcvk-nbd-iscsi-test"
PLUGIN_PATH="/var/tmp/bcvk/libnbdkit_erofs_plugin.so"
IQN="iqn.2025-05.dev.bcvk:ephemeral"

MACHINE=""
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
        podman machine ssh "$MACHINE" -- bash -c '
            nbd-client -d /dev/nbd0 2>/dev/null
            tgt-admin --force --delete ALL 2>/dev/null
            killall tgtd 2>/dev/null
        ' 2>/dev/null || true

        podman machine ssh "$MACHINE" -- podman rm -f '"$NBD_CONTAINER"' 2>/dev/null || true

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
        ((PASS++))
    else
        echo "  FAIL  $label (expected: $expected, got: $result)"
        ((FAIL++))
    fi
}

run_ssh() {
    ssh -p "$SSH_PORT" -i "$SSH_KEY" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout=2 \
        root@localhost "$@" 2>/dev/null
}

# ── Phase 0: Prerequisites ──────────────────────────────────────────

echo "=== Phase 0: Prerequisites ==="

echo -n "  qemu-system-aarch64... "
command -v qemu-system-aarch64 >/dev/null || { echo "MISSING (brew install qemu)"; exit 1; }
if qemu-system-aarch64 -drive driver=help 2>&1 | grep -q iscsi; then
    echo "OK (with libiscsi)"
else
    echo "MISSING libiscsi (brew install libiscsi && brew reinstall --build-from-source qemu)"
    exit 1
fi

echo -n "  EFI firmware... "
[[ -f "$EFI_CODE" ]] || { echo "MISSING ($EFI_CODE)"; exit 1; }
echo "OK"

echo -n "  Podman machine... "
MACHINE=$(podman machine info --format '{{.Host.CurrentMachine}}' 2>/dev/null)
[[ -n "$MACHINE" ]] || { echo "MISSING (no running machine)"; exit 1; }
echo "OK ($MACHINE)"

echo -n "  rootful check... "
if [[ "$(podman machine ssh "$MACHINE" -- id -u 2>/dev/null)" == "0" ]]; then
    ROOTFUL=true; echo "rootful"
else
    ROOTFUL=false; echo "rootless"
fi

echo -n "  packages (nbd, tgtd)... "
podman machine ssh "$MACHINE" -- bash -c '
    rpm -q nbd scsi-target-utils &>/dev/null || dnf install -y nbd scsi-target-utils &>/dev/null
' 2>/dev/null
echo "OK"

echo -n "  nbd kernel module... "
podman machine ssh "$MACHINE" -- bash -c '
    lsmod | grep -q "^nbd " || modprobe nbd max_part=8
' 2>/dev/null
echo "OK"

echo -n "  /dev/nbd0... "
podman machine ssh "$MACHINE" -- test -b /dev/nbd0 2>/dev/null || { echo "MISSING"; exit 1; }
echo "OK"

echo -n "  erofs plugin... "
podman machine ssh "$MACHINE" -- test -f "$PLUGIN_PATH" 2>/dev/null || { echo "MISSING ($PLUGIN_PATH)"; exit 1; }
echo "OK"

echo -n "  SSH keygen... "
rm -f "$SSH_KEY" "${SSH_KEY}.pub"
ssh-keygen -t ed25519 -f "$SSH_KEY" -N "" -q
SSH_PUBKEY=$(cat "${SSH_KEY}.pub")
echo "OK"

# ── Phase 1: iSCSI target setup ─────────────────────────────────────

echo ""
echo "=== Phase 1: iSCSI target setup (Podman Machine) ==="

echo -n "  image pull... "
podman image exists "$TEST_IMAGE" 2>/dev/null || podman pull -q "$TEST_IMAGE" >/dev/null
echo "OK"

echo -n "  image mount... "
if $ROOTFUL; then
    MERGED=$(podman machine ssh "$MACHINE" -- podman image mount "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')
else
    MERGED=$(podman machine ssh "$MACHINE" -- podman unshare podman image mount "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')
fi
[[ -n "$MERGED" ]] || { echo "FAILED"; exit 1; }
echo "OK ($MERGED)"

echo -n "  nbdkit erofs plugin... "
podman machine ssh "$MACHINE" -- podman rm -f "$NBD_CONTAINER" &>/dev/null || true
CMDLINE="root=/dev/sda2 ro rootfstype=erofs console=ttyAMA0 console=tty0 loglevel=4 selinux=0 net.ifnames=0 systemd.journald.storage=volatile"
podman machine ssh "$MACHINE" -- bash -c "
    podman run -d --name $NBD_CONTAINER --security-opt label=disable \
        -v $MERGED:$MERGED:ro \
        -v $PLUGIN_PATH:/plugin.so:ro \
        -v /usr/bin/nbdkit:/usr/bin/nbdkit:ro \
        -v /usr/lib64/nbdkit:/usr/lib64/nbdkit:ro \
        quay.io/fedora/fedora:latest \
        nbdkit -f -p 10809 -r /plugin.so \
        dir=$MERGED \
        'cmdline=$CMDLINE' \
        'ssh_pubkey=$SSH_PUBKEY'
" &>/dev/null
echo "OK"

echo -n "  nbdkit ready... "
DEADLINE=$((SECONDS + 30))
NBD_READY=false
while [[ $SECONDS -lt $DEADLINE ]]; do
    if podman machine ssh "$MACHINE" -- bash -c "
        python3 -c \"
import socket, sys
s = socket.socket(); s.settimeout(1)
try:
    s.connect(('127.0.0.1', 10809)); data = s.recv(8)
    sys.exit(0 if data == b'NBDMAGIC' else 1)
except: sys.exit(1)
\"" 2>/dev/null; then
        NBD_READY=true; break
    fi
    sleep 1
done
$NBD_READY || { echo "TIMEOUT"; podman machine ssh "$MACHINE" -- podman logs "$NBD_CONTAINER" 2>/dev/null | tail -10; exit 1; }
echo "OK"

echo -n "  nbd-client → /dev/nbd0... "
podman machine ssh "$MACHINE" -- nbd-client localhost 10809 /dev/nbd0 -readonly -name "" 2>/dev/null
NBD_SIZE=$(podman machine ssh "$MACHINE" -- blockdev --getsize64 /dev/nbd0 2>/dev/null | tr -d '\r\n')
echo "OK (${NBD_SIZE} bytes)"

echo -n "  tgtd + iSCSI target... "
podman machine ssh "$MACHINE" -- bash -c "
    killall tgtd 2>/dev/null || true
    sleep 1
    tgtd &
    sleep 2
    tgtadm --lld iscsi --op new --mode target --tid 1 -T $IQN
    tgtadm --lld iscsi --op new --mode logicalunit --tid 1 --lun 1 -b /dev/nbd0 --bstype rdwr --readonly 1
    tgtadm --lld iscsi --op bind --mode target --tid 1 -I ALL
" 2>/dev/null
echo "OK"

echo -n "  iSCSI discovery (inside PM)... "
DISCOVERY=$(podman machine ssh "$MACHINE" -- iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260 2>/dev/null | tr -d '\r\n')
echo "$DISCOVERY" | grep -q "$IQN" || { echo "FAIL ($DISCOVERY)"; exit 1; }
echo "OK ($DISCOVERY)"

# ── Phase 2: Port forward + QEMU boot ───────────────────────────────

echo ""
echo "=== Phase 2: QEMU iSCSI boot ==="

echo -n "  SSH tunnel (3260)... "
podman machine ssh "$MACHINE" -L ${ISCSI_PORT}:localhost:${ISCSI_PORT} -N &>/dev/null &
TUNNEL_PID=$!
sleep 2
kill -0 "$TUNNEL_PID" 2>/dev/null || { echo "FAIL (tunnel died)"; exit 1; }
echo "OK (PID $TUNNEL_PID)"

echo -n "  EFI vars... "
dd if=/dev/zero of="$EFI_VARS" bs=1m count=64 2>/dev/null
echo "OK"

echo -n "  QEMU start... "
qemu-system-aarch64 \
    -machine virt -accel hvf -cpu host \
    -m 4096 -smp 4 \
    -drive if=pflash,format=raw,readonly=on,file="$EFI_CODE" \
    -drive if=pflash,format=raw,file="$EFI_VARS" \
    -drive driver=iscsi,transport=tcp,portal=127.0.0.1:${ISCSI_PORT},target="$IQN",lun=1,readonly=on,if=none,id=disk0 \
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
kill -0 "$QEMU_PID" 2>/dev/null || { echo "FAIL (QEMU exited)"; cat "$LOG_DIR/qemu-stdout.log" | tail -20; exit 1; }
echo "OK (PID $QEMU_PID)"

echo -n "  SSH wait (up to 240s)... "
SSH_READY=false
DEADLINE=$((SECONDS + 240))
ATTEMPT=0
while [[ $SECONDS -lt $DEADLINE ]]; do
    kill -0 "$QEMU_PID" 2>/dev/null || { echo "FAIL (QEMU died)"; tail -40 "$LOG_DIR/qemu-serial.log"; exit 1; }
    if run_ssh true; then SSH_READY=true; break; fi
    if   [[ $ATTEMPT -lt 2 ]]; then sleep 1
    elif [[ $ATTEMPT -lt 4 ]]; then sleep 2
    else sleep 3; fi
    ((ATTEMPT++))
done
$SSH_READY || { echo "TIMEOUT"; tail -60 "$LOG_DIR/qemu-serial.log"; exit 1; }
echo "OK (${SECONDS}s, attempt $ATTEMPT)"

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
