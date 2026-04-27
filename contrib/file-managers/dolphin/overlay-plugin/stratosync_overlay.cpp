/*
 * Stratosync KOverlayIconPlugin implementation.
 *
 * See header for the threading rationale. Emblem icon names mirror
 * stratosync_fm_common.py's EMBLEM_MAP exactly so a sync-status change
 * looks identical across desktops.
 */
#include "stratosync_overlay.h"

#include <QFile>
#include <QMutexLocker>
#include <QString>
#include <QStringList>
#include <QThreadPool>
#include <QUrl>
#include <QtConcurrent/QtConcurrent>

#include <sys/types.h>
#include <sys/xattr.h>

namespace {

// Buffer is sized for the longest legal status string ("hydrating" = 9
// chars) with margin. The FUSE layer never writes anything larger than 16.
constexpr ssize_t kStatusBufSize = 32;
constexpr const char *kStatusXattr = "user.stratosync.status";

QString readStatus(const QString &path) {
    const QByteArray pathBytes = QFile::encodeName(path);
    char buf[kStatusBufSize];
    ssize_t n = ::getxattr(pathBytes.constData(), kStatusXattr,
                           buf, sizeof(buf) - 1);
    if (n <= 0) {
        return QString();   // not a stratosync-managed path
    }
    return QString::fromUtf8(buf, static_cast<int>(n));
}

QStringList emblemsFor(const QString &status) {
    // Mirror stratosync_fm_common.EMBLEM_MAP exactly.
    if (status == QStringLiteral("cached"))    return {QStringLiteral("emblem-default")};
    if (status == QStringLiteral("dirty"))     return {QStringLiteral("emblem-synchronizing")};
    if (status == QStringLiteral("uploading")) return {QStringLiteral("emblem-synchronizing")};
    if (status == QStringLiteral("hydrating")) return {QStringLiteral("emblem-downloads")};
    if (status == QStringLiteral("remote"))    return {QStringLiteral("emblem-web")};
    if (status == QStringLiteral("conflict"))  return {QStringLiteral("emblem-important")};
    if (status == QStringLiteral("stale"))     return {QStringLiteral("emblem-generic")};
    return {};
}

}  // namespace

StratosyncOverlayPlugin::StratosyncOverlayPlugin(QObject *parent)
    : KOverlayIconPlugin(parent) {}

QStringList StratosyncOverlayPlugin::getOverlays(const QUrl &item) {
    // Reject anything we can't getxattr() on. Network URLs, trash:/, etc.
    if (!item.isLocalFile()) {
        return {};
    }
    const QString path = item.toLocalFile();
    if (path.isEmpty()) {
        return {};
    }

    {
        QMutexLocker lock(&m_mutex);
        if (m_inFlight.contains(path)) {
            // A worker is already computing the answer for this path; it
            // will emit overlaysChanged() shortly.
            return {};
        }
        m_inFlight.insert(path);
    }

    // Run the xattr read off the GUI thread. ENODATA on non-stratosync
    // paths is a few µs even via FUSE, but a slow or stuck daemon must
    // never freeze Dolphin — that's the whole point of this dispatch.
    (void) QtConcurrent::run(QThreadPool::globalInstance(),
                             [this, item]() { computeAndEmit(item); });

    return {};
}

void StratosyncOverlayPlugin::computeAndEmit(const QUrl &item) {
    // Runs on a QThreadPool worker.
    const QString path = item.toLocalFile();
    const QString status = readStatus(path);
    const QStringList overlays = emblemsFor(status);

    {
        QMutexLocker lock(&m_mutex);
        m_inFlight.remove(path);
    }

    if (overlays.isEmpty()) {
        // Either the path isn't stratosync-managed (no xattr) or its
        // status isn't one we know. Don't emit — Dolphin's initial
        // empty response already covers "no overlays".
        return;
    }

    // Q_EMIT from a worker thread is safe: KIO's KCoreDirLister connects
    // to overlaysChanged() with Qt::AutoConnection, which becomes a
    // queued connection across threads.
    Q_EMIT overlaysChanged(item, overlays);
}

#include "moc_stratosync_overlay.cpp"
