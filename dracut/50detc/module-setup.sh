#!/bin/bash

# `moddir`, `initdir`, `systemdsystemunitdir`, `hostonly`, `SYSTEMCTL` and the
# `detc_*` settings are dracut's own, set in the shell that sources this, so
# there is nothing here that could assign them
# shellcheck disable=SC2154

# Put detc, everything it is made of, and everything it shells out to, into an
# initrd.
#
# Bash and not `#!/bin/sh`, unlike every other executable in this repository:
# dracut sources this file into a bash shell and calls the four functions
# below, so the shell is not this script's to choose.  Nothing here runs on a
# node -- it runs on the machine that builds an image -- which is also why it is
# the one place allowed to depend on tools that no node needs.
#
# ## What ends up in the image
#
# The binary, the five kinds of object it reads, the driver with the delivery
# sources it runs, its unit, and then the awkward part: the programs the shipped
# probes and providers call.  Those are a real dependency and there is no way
# around it -- a provider is a shell script by design, so `useradd` in the
# initrd is what `resources.d/user/...` converging before switch-root actually
# means.  They are grouped below by what asks for them, so that a fleet cutting
# the image down knows what it is giving up.
#
# `detc` itself brings nothing: TLS and the certificate authorities are compiled
# into it, so a bundle can be fetched over https without `curl`, without a
# certificate bundle and without anything from OpenSSL.

check() {
    require_binaries detc || return 1

    # 255 is "only when asked for".  This module roughly doubles a minimal
    # initrd, and an image that is not going to be configured this way should
    # not pay for it; `--add detc`, or `add_dracutmodules+=" detc "` in
    # `/etc/dracut.conf.d/`, is the opt in
    return 255
}

depends() {
    echo systemd

    # Only when the fleet says so.  The network stack is the largest single
    # thing this could pull in, and three of the four sources deliver a bundle
    # without it; `detc_network=yes` in `/etc/dracut.conf.d/detc.conf` is for
    # the fleet that hands out a URL, and goes with `rd.neednet=1` at boot
    if [[ ${detc_network-} == yes ]]; then
        echo network
    fi

    return 0
}

installkernel() {
    # For `inject/40-volume`, and for nothing else here, so a fleet that leaves
    # that source out gets an image without them.  ISO 9660 and FAT are the two
    # filesystems that every tool can write a seed image with, and the NLS
    # modules are what the kernel wants before it will read a name out of either.
    #
    # This is the payoff of `detc_omit_sources=` over deleting the file: four
    # kernel modules are worth more than the script that needs them.  The
    # userspace side is not conditional to match -- `blkid`, `mount` and `umount`
    # are in most initrds already and another module may be counting on them
    if [[ " ${detc_omit_sources-} " != *" 40-volume "* ]]; then
        instmods isofs vfat nls_cp437 nls_iso8859-1
    fi
}

# A whole directory, at the same path, with the modes it has.
#
# dracut has no helper for this, and the obvious substitute -- a glob passed to
# `inst_multiple` -- is wrong twice over: it is the building machine's shell that
# expands it, so on a cross build it matches the wrong tree entirely, and the
# depth of the tree ends up written into the module as one glob per level.  The
# objects here are nested three deep in places and a fleet's are nested however
# deep the fleet likes.
detc_tree() {
    local sysroot="${dracutsysrootdir-}"
    local dir="$1" file

    [[ -d ${sysroot}${dir} ]] || return 0

    inst_dir "$dir"
    while read -r file; do
        inst_simple "${file#"$sysroot"}"
    done < <(find "${sysroot}${dir}" \( -type f -o -type l \))
}

# The delivery sources, which are the one part of this a fleet routinely wants
# less of.  A flat directory rather than a tree, and a filtered one:
# `detc_omit_sources=` in `/etc/dracut.conf.d/detc.conf` names the ones to leave
# out, by filename, separated by spaces.
#
# Leaving one out is the whole mechanism, and it is not masking.  There is no
# prefix ladder under `inject/` for a higher prefix to mask from -- detc does not
# read that directory and `tools/detc-inject` reads the single path it was
# installed to -- so the moment to decide is while the image is being built.
# That is also the better moment: a source that is not in the image costs no
# bytes and no boot time, where a masked one costs both.
#
# `detc-inject` still skips a zero length file, which stays the ad hoc route for
# an image built by something that cannot write a dracut configuration.
detc_sources() {
    local sysroot="${dracutsysrootdir-}"
    local dir=/usr/libexec/detc/inject
    local omit=" ${detc_omit_sources-} "
    local source name

    [[ -d ${sysroot}${dir} ]] || return 0

    inst_dir "$dir"
    for source in "${sysroot}${dir}"/*; do
        [[ -f $source || -L $source ]] || continue

        name=${source##*/}
        if [[ $omit == *" $name "* ]]; then
            # Taken out of the list as it is honoured, so that whatever is left
            # over at the end is what matched nothing
            omit=${omit/" $name "/" "}
            dinfo "detc: leaving out the delivery source $name"
            continue
        fi

        inst_simple "${source#"$sysroot"}"
    done

    # A name that matched nothing is a typo, and a typo here is a source the
    # fleet believes it turned off and did not.  A warning and not an error: an
    # image that still builds is worth more than one that is exactly as asked
    for name in $omit; do
        dwarn "detc: detc_omit_sources names $name, which is not a delivery source"
    done
}

install() {
    inst_binary /usr/bin/detc

    # The five kinds of object, at the paths they are installed to, because those
    # are the paths detc searches and the ones `tools/detc-inject` copies from
    local tree
    for tree in /usr/share/detc/templates.d \
                /usr/share/detc/resources.d \
                /usr/share/detc/variables \
                /usr/libexec/detc/probes \
                /usr/libexec/detc/providers.d; do
        detc_tree "$tree"
    done

    # And `inject/`, which is not one of them: it is the driver's own directory
    # of delivery sources, nothing in detc reads it, and it is the one a fleet
    # can ask for less of
    detc_sources

    # The driver, and the stand in provider that it installs into `/sysroot`
    inst_script /usr/libexec/detc/detc-inject
    inst_script /usr/libexec/detc/detc-defer

    # Trust, so that a bundle can be signed even when the image being booted has
    # never had detc installed.  `tools/detc-inject` only ever seeds this into a
    # root that has nothing, and never over anything
    inst_simple -o /etc/detc/allowed_signers

    inst_simple "${moddir}/detc-inject.service" \
                "${systemdsystemunitdir}/detc-inject.service"
    $SYSTEMCTL -q --root "${initdir}" enable detc-inject.service

    # What the shipped assets call.  `sh` and the shell built-ins are dracut's
    # `base`; these are the rest, taken from reading the eight probes and the
    # eight providers rather than from guessing
    inst_multiple sh awk sed tr cat head printf id stat readlink dirname \
                  chmod chown mkdir rmdir rm ln install mktemp

    # Probes.  `ip` comes with the network module and `systemd-detect-virt` with
    # systemd, so both are optional here and are asked for anyway in case
    # neither module was pulled in
    inst_multiple -o lsblk ip systemd-detect-virt

    # Sources.  `dmidecode` is only a nicety -- `inject/30-smbios` reads the
    # same table out of sysfs when it is not there -- and the rest is what
    # `40-volume` needs to find and read a seed image
    inst_multiple -o dmidecode
    inst_multiple blkid mount umount

    # Accounts and keys, which is most of what a fleet asks a first boot for and
    # is also the heaviest thing in this list.  `detc_accounts=no` in
    # `/etc/dracut.conf.d/detc.conf` is for an image whose users are baked in
    if [[ ${detc_accounts-yes} == yes ]]; then
        inst_multiple -o useradd usermod userdel groupadd groupmod groupdel \
                         getent chpasswd ssh-keygen
        inst_simple -o /etc/login.defs
        inst_simple -o /etc/default/useradd
    fi
}
