#!/bin/sh
# /sbin/overlay-init — PID 1 under a read-only rootfs. Build a tmpfs-backed
# overlay (RO root = lowerdir) and pivot into BusyBox /sbin/init.
set -e
mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev 2>/dev/null || true

# A cold room may supply a private second virtio drive. Fail the boot if it
# cannot mount: falling back to RAM would silently violate the requested capacity.
if grep -qw 'rooms.scratch=1' /proc/cmdline; then
  mount -t ext4 /dev/vdb /mnt
fi
if ! grep -qw 'rooms.scratch=1' /proc/cmdline; then
  mount -t tmpfs tmpfs /mnt
fi
mkdir -p /mnt/upper /mnt/work /mnt/newroot
mount -t overlay overlay \
  -o lowerdir=/,upperdir=/mnt/upper,workdir=/mnt/work \
  /mnt/newroot

# Toolchains carry their own dynamic loaders and libc under /nix/store.
# Mount the complete closure outside the writable overlay. Any failure aborts
# before SSH starts; no fallback to an incomplete PATH is allowed.
for arg in $(cat /proc/cmdline); do
  case "$arg" in
    rooms.toolstore=vdb|rooms.toolstore=vdc)
      mkdir -p /mnt/newroot/nix
      mount -t squashfs -o ro "/dev/${arg#rooms.toolstore=}" /mnt/newroot/nix
      test -d /mnt/newroot/nix/var/rooms
      ;;
  esac
done

# A base boots without an interactive surface. These changes live only in the
# tmpfs upper layer; ordinary rooms run boots retain the image's sshd/getty.
if grep -qw 'rooms.base=1' /proc/cmdline; then
  rm -f /mnt/newroot/etc/runlevels/default/sshd
  rm -f /mnt/newroot/etc/runlevels/boot/rooms-secrets
  sed -i '/ttyS0::respawn/d' /mnt/newroot/etc/inittab
fi

# Move the live pseudo-fs into the new root; keep the old RO root reachable as
# /oldroot (it is the overlay lowerdir, so it MUST stay mounted).
mkdir -p /mnt/newroot/proc /mnt/newroot/sys /mnt/newroot/dev /mnt/newroot/oldroot
mount --move /proc /mnt/newroot/proc
mount --move /sys  /mnt/newroot/sys
mount --move /dev  /mnt/newroot/dev

cd /mnt/newroot
pivot_root . oldroot                 # old RO root + the tmpfs land under /oldroot
exec chroot . /sbin/init </dev/console >/dev/console 2>&1
