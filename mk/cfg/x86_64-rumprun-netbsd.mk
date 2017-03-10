# x86_64-rumprun-netbsd configuration
CROSS_PREFIX_x86_64-rumprun-netbsd=x86_64-rumprun-netbsd-
CC_x86_64-rumprun-netbsd=gcc
CXX_x86_64-rumprun-netbsd=g++
CPP_x86_64-rumprun-netbsd=gcc -E
AR_x86_64-rumprun-netbsd=ar
CFG_INSTALL_ONLY_RLIB_x86_64-rumprun-netbsd = 1
CFG_LIB_NAME_x86_64-rumprun-netbsd=lib$(1).so
CFG_STATIC_LIB_NAME_x86_64-rumprun-netbsd=lib$(1).a
CFG_LIB_GLOB_x86_64-rumprun-netbsd=lib$(1)-*.so
CFG_JEMALLOC_CFLAGS_x86_64-rumprun-netbsd := -m64
CFG_GCCISH_CFLAGS_x86_64-rumprun-netbsd :=  -g -fPIC -m64
CFG_GCCISH_CXXFLAGS_x86_64-rumprun-netbsd :=
CFG_GCCISH_LINK_FLAGS_x86_64-rumprun-netbsd :=
CFG_GCCISH_DEF_FLAG_x86_64-rumprun-netbsd :=
CFG_LLC_FLAGS_x86_64-rumprun-netbsd :=
CFG_INSTALL_NAME_x86_64-rumprun-netbsd =
CFG_EXE_SUFFIX_x86_64-rumprun-netbsd =
CFG_WINDOWSY_x86_64-rumprun-netbsd :=
CFG_UNIXY_x86_64-rumprun-netbsd := 1
CFG_LDPATH_x86_64-rumprun-netbsd :=
CFG_RUN_x86_64-rumprun-netbsd=$(2)
CFG_RUN_TARG_x86_64-rumprun-netbsd=$(call CFG_RUN_x86_64-rumprun-netbsd,,$(2))
CFG_GNU_TRIPLE_x86_64-rumprun-netbsd := x86_64-rumprun-netbsd
CFG_DISABLE_JEMALLOC_x86_64-rumprun-netbsd := 1