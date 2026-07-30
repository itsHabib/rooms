#!/bin/sh
# /sbin/overlay-init — PID 1 under a read-only rootfs. Build a tmpfs-backed
# overlay (RO root = lowerdir) and pivot into BusyBox /sbin/init.
set -e
mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev 2>/dev/null || true

mount -t tmpfs tmpfs /mnt            # /mnt exists in the image; holds upper+work+newroot
mkdir -p /mnt/upper /mnt/work /mnt/newroot
mount -t overlay overlay \
  -o lowerdir=/,upperdir=/mnt/upper,workdir=/mnt/work \
  /mnt/newroot

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
