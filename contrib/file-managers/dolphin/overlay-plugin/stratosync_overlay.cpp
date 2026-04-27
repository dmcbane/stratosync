/*
 * Stratosync KOverlayIconPlugin implementation.
 *
 * Reads the `user.stratosync.status` xattr the FUSE layer exposes on every
 * managed file and maps to the same freedesktop emblem names that the
 * Nautilus/Nemo/Caja Python extensions use. Emblem icon names are kept in
 * sync with stratosync_fm_common.py — see that file's EMBLEM_MAP for the
 * canonical list.
 */
#include "stratosync_overlay.h"

#include <QFile>
#include <QString>
#include <QUrl>

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

QString emblemFor(const QString &status) {
    // Mirror stratosync_fm_common.EMBLEM_MAP exactly.
    if (status == QStringLiteral("cached"))    return QStringLiteral("emblem-default");
    if (status == QStringLiteral("dirty"))     return QStringLiteral("emblem-synchronizing");
    if (status == QStringLiteral("uploading")) return QStringLiteral("emblem-synchronizing");
    if (status == QStringLiteral("hydrating")) return QStringLiteral("emblem-downloads");
    if (status == QStringLiteral("remote"))    return QStringLiteral("emblem-web");
    if (status == QStringLiteral("conflict"))  return QStringLiteral("emblem-important");
    if (status == QStringLiteral("stale"))     return QStringLiteral("emblem-generic");
    return QString();
}

}  // namespace

StratosyncOverlayPlugin::StratosyncOverlayPlugin(QObject *parent)
    : KOverlayIconPlugin(parent) {}

QStringList StratosyncOverlayPlugin::getOverlays(const QUrl &item) {
    if (!item.isLocalFile()) {
        return {};
    }
    const QString path = item.toLocalFile();
    const QString status = readStatus(path);
    if (status.isEmpty()) {
        return {};
    }
    const QString emblem = emblemFor(status);
    if (emblem.isEmpty()) {
        return {};
    }
    return {emblem};
}

#include "moc_stratosync_overlay.cpp"
