/*
 * Stratosync KOverlayIconPlugin — sync-status emblems for Dolphin/Konqueror.
 *
 * This is a Qt6/KF6 plugin loaded by KIO when Dolphin lists files. For each
 * URL it sees, it returns a list of standard freedesktop emblem icon names
 * (matching the Nautilus/Nemo/Caja Python extensions) based on the
 * `user.stratosync.status` xattr the FUSE layer exposes.
 *
 * Emblem names mirror the GTK extensions exactly so a status change looks
 * identical across desktops.
 */
#pragma once

#include <KOverlayIconPlugin>

class StratosyncOverlayPlugin : public KOverlayIconPlugin {
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.KOverlayIconPlugin" FILE "stratosync_overlay.json")

public:
    explicit StratosyncOverlayPlugin(QObject *parent = nullptr);

    // Called synchronously by Dolphin per file. Must be cheap — we read
    // a single xattr (handled by the FUSE layer's in-memory state DB) and
    // map to an emblem name. Non-stratosync paths return empty.
    QStringList getOverlays(const QUrl &item) override;
};
