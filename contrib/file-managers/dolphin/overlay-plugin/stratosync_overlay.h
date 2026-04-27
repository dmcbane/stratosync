/*
 * Stratosync KOverlayIconPlugin — sync-status emblems for Dolphin/Konqueror.
 *
 * KOverlayIconPlugin::getOverlays() is called from Dolphin's GUI thread
 * once per file when a directory is listed. Doing the xattr read inline
 * means an unresponsive FUSE daemon (or any backend hiccup) directly
 * freezes the UI — a directory with 5000 entries × any per-file slowness
 * is a hung Dolphin.
 *
 * Strategy: return empty from getOverlays() immediately, dispatch the
 * actual read to QThreadPool::globalInstance(), and emit overlaysChanged()
 * from the worker once the result is in. KIO connects to the signal via
 * Qt::AutoConnection, so cross-thread emit is handled correctly. Repeated
 * calls for the same in-flight path are deduped via a small in-memory set.
 */
#pragma once

#include <KOverlayIconPlugin>
#include <QMutex>
#include <QSet>
#include <QString>

class StratosyncOverlayPlugin : public KOverlayIconPlugin {
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.KOverlayIconPlugin" FILE "stratosync_overlay.json")

public:
    explicit StratosyncOverlayPlugin(QObject *parent = nullptr);

    // Returns empty immediately. Schedules the xattr read on a worker
    // thread, then emits overlaysChanged() with the result.
    QStringList getOverlays(const QUrl &item) override;

private:
    void computeAndEmit(const QUrl &item);

    QMutex        m_mutex;       // guards m_inFlight only
    QSet<QString> m_inFlight;    // paths currently being computed
};
