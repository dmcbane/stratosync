cmake_minimum_required(VERSION 3.16)
project(stratosync-overlay-plugin VERSION 0.12.0 LANGUAGES CXX)

#
# Stratosync Dolphin/Konqueror sync-status emblem plugin (KF6 KOverlayIconPlugin).
#
# Build:
#   cmake -B build -S .
#   cmake --build build
#   sudo cmake --install build
#
# The .so installs into KF6's plugin directory:
#   /usr/lib*/qt6/plugins/kf6/overlayicon/stratosyncoverlay.so
# Dolphin discovers it on startup. Restart Dolphin (or `kbuildsycoca6`) to
# refresh the plugin list after install.
#

set(QT_MIN_VERSION 6.5.0)
set(KF_MIN_VERSION 6.0.0)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

# ECM (Extra CMake Modules) ships with kf6-extra-cmake-modules / extra-cmake-modules.
# It provides KDEInstallDirs, KDECompilerSettings, etc. — the standard
# CMake helpers every KF6 build expects.
find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
list(APPEND CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(KDECMakeSettings)

find_package(Qt6 ${QT_MIN_VERSION} REQUIRED COMPONENTS Core Concurrent Gui)
find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS KIO)

add_library(stratosyncoverlay MODULE
    stratosync_overlay.cpp
    stratosync_overlay.h
)

# AUTOMOC needs to find stratosync_overlay.json next to the source.
target_compile_definitions(stratosyncoverlay PRIVATE
    QT_NO_KEYWORDS
)

target_link_libraries(stratosyncoverlay PRIVATE
    Qt6::Core
    Qt6::Concurrent
    Qt6::Gui
    KF6::KIOCore
    KF6::KIOGui
)

# KOverlayIconPlugin lives under <pluginsdir>/kf6/overlayicon/. KDE's
# KDE_INSTALL_PLUGINDIR resolves to e.g. /usr/lib64/qt6/plugins on Fedora-
# family or /usr/lib/x86_64-linux-gnu/qt6/plugins on Debian.
install(TARGETS stratosyncoverlay
    DESTINATION ${KDE_INSTALL_PLUGINDIR}/kf6/overlayicon
)

message(STATUS "Stratosync overlay plugin will install to: "
        "${KDE_INSTALL_PLUGINDIR}/kf6/overlayicon")
