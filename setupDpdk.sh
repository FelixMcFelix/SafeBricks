#!/bin/bash


#sudo bash ../utils/vm-kernel-upgrade.sh
#require rebooting

#sudo bash ../utils/vm-setup.sh

DPDK_HOME=~/tools/dpdk-stable-20.11.3

# add -DHG_MON=1 if you want dpdk to print memzone info.
CFLAGS="-g3 -Wno-error=maybe-uninitialized -fPIC"

sudo apt-get -y install build-essential ca-certificates curl \
    libnuma-dev libpcap-dev xz-utils

# This is used when you want to monitor dpdk hugepage usage during runtime.
build_dpdk_hugepage_mon () {
    if [ ! -d "dpdk-stable-20.11.3" ]; then
        curl -sSf https://fast.dpdk.org/rel/dpdk-20.11.3.tar.xz | tar -xJv
    elif [ ! -d "dpdk-stable-20.11.3/.git" ]; then
        sudo rm -rf dpdk-stable-20.11.3/
        git clone git@github.com:YangZhou1997/dpdk-stable-20.11.3.git
    else
        echo "Rebuild dpdk!"
    fi
}

# This is used when you normally want to rebuild dpdk in case that you made some modification.
build_dpdk_normal () {
    if [ ! -d "dpdk-stable-20.11.3" ]; then
        curl -sSf https://fast.dpdk.org/rel/dpdk-20.11.3.tar.xz | tar -xJv
    else
        echo "Just build!"
    fi
}

cd ~/tools
# build_dpdk_hugepage_mon
build_dpdk_normal


cp ~/utils/dpdk/common_linuxapp-20.11 $DPDK_HOME/config/common_linuxapp

cd $DPDK_HOME

#make clean
#make config T=x86_64-native-linuxapp-gcc EXTRA_CFLAGS="${CFLAGS}"
#make -j16 EXTRA_CFLAGS="${CFLAGS}"
#sudo make install
meson build
ninja -C build
ninja -C build install

sudo insmod $DPDK_HOME/build/kmod/igb_uio.ko

sudo $DPDK_HOME/usertools/dpdk-devbind.py --force -b igb_uio 0000:00:1f.6

bash ~/NetBricks/setupDpdkCopy.sh

# hugepages setup on numa node
echo 2048 | sudo tee /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages
echo 2048 | sudo tee /sys/devices/system/node/node1/hugepages/hugepages-2048kB/nr_hugepages

echo "please rebuild NetBricks to make dpdk changes valid"
