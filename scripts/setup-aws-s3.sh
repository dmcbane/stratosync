#!/usr/bin/env bash
#
# setup-aws-s3.sh — Create an S3 bucket, IAM user with scoped access,
# configure rclone, and add the mount to stratosync config.
#
# Prerequisites:
#   - aws cli v2 configured with admin-level credentials
#   - rclone installed
#   - jq installed
#
# Usage:
#   ./scripts/setup-aws-s3.sh [OPTIONS]
#
# Options:
#   -b, --bucket NAME       S3 bucket name (required)
#   -r, --region REGION     AWS region (default: us-east-1)
#   -u, --user NAME         IAM user name (default: stratosync-<bucket>)
#   -n, --rclone-remote NAME  rclone remote name (default: s3-<bucket>)
#   -m, --mount-path PATH   Local mount path (default: ~/S3-<bucket>)
#   -q, --cache-quota SIZE  Cache quota (default: 10 GiB)
#   -p, --poll-interval DUR Poll interval (default: 120s)
#       --dry-run           Print commands without executing
#   -h, --help              Show this help

set -euo pipefail

# --- Defaults ---
REGION="us-east-1"
BUCKET=""
IAM_USER=""
RCLONE_REMOTE=""
MOUNT_PATH=""
CACHE_QUOTA="10 GiB"
POLL_INTERVAL="120s"
DRY_RUN=false
STRATOSYNC_CONFIG="${STRATOSYNC_CONFIG:-$HOME/.config/stratosync/config.toml}"

# --- Helpers ---
die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "==> $*"; }
warn() { echo "WARNING: $*" >&2; }

run() {
    if $DRY_RUN; then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

usage() {
    sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# //'
    sed -n '/^# Options:/,/^[^#]/p' "$0" | sed 's/^# //'
    exit 0
}

# --- Parse args ---
while [[ $# -gt 0 ]]; do
    case "$1" in
        -b|--bucket)        BUCKET="$2"; shift 2 ;;
        -r|--region)        REGION="$2"; shift 2 ;;
        -u|--user)          IAM_USER="$2"; shift 2 ;;
        -n|--rclone-remote) RCLONE_REMOTE="$2"; shift 2 ;;
        -m|--mount-path)    MOUNT_PATH="$2"; shift 2 ;;
        -q|--cache-quota)   CACHE_QUOTA="$2"; shift 2 ;;
        -p|--poll-interval) POLL_INTERVAL="$2"; shift 2 ;;
        --dry-run)          DRY_RUN=true; shift ;;
        -h|--help)          usage ;;
        *)                  die "Unknown option: $1" ;;
    esac
done

[[ -n "$BUCKET" ]] || die "Bucket name is required. Use -b/--bucket."

# Derive defaults from bucket name
IAM_USER="${IAM_USER:-stratosync-${BUCKET}}"
RCLONE_REMOTE="${RCLONE_REMOTE:-s3-${BUCKET}}"
MOUNT_PATH="${MOUNT_PATH:-$HOME/S3-${BUCKET}}"

# --- Pre-flight checks ---
for cmd in aws rclone jq; do
    command -v "$cmd" &>/dev/null || die "'$cmd' is not installed."
done

aws sts get-caller-identity &>/dev/null || die "AWS CLI is not configured or credentials are expired."

info "Configuration:"
echo "  Bucket:          $BUCKET"
echo "  Region:          $REGION"
echo "  IAM User:        $IAM_USER"
echo "  rclone Remote:   $RCLONE_REMOTE"
echo "  Mount Path:      $MOUNT_PATH"
echo "  Cache Quota:     $CACHE_QUOTA"
echo "  Poll Interval:   $POLL_INTERVAL"
echo "  Stratosync Cfg:  $STRATOSYNC_CONFIG"
echo ""

if ! $DRY_RUN; then
    read -rp "Proceed? [y/N] " confirm
    [[ "$confirm" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 0; }
    echo ""
fi

# ============================================================
# 1. Create S3 bucket
# ============================================================
info "Creating S3 bucket: $BUCKET (region: $REGION)"

if aws s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    warn "Bucket '$BUCKET' already exists, skipping creation."
else
    if [[ "$REGION" == "us-east-1" ]]; then
        run aws s3api create-bucket \
            --bucket "$BUCKET" \
            --region "$REGION"
    else
        run aws s3api create-bucket \
            --bucket "$BUCKET" \
            --region "$REGION" \
            --create-bucket-configuration LocationConstraint="$REGION"
    fi
fi

# Enable versioning (useful for conflict detection)
info "Enabling bucket versioning"
run aws s3api put-bucket-versioning \
    --bucket "$BUCKET" \
    --versioning-configuration Status=Enabled

# Block public access
info "Blocking all public access"
run aws s3api put-public-access-block \
    --bucket "$BUCKET" \
    --public-access-block-configuration \
        BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true

# Enable default encryption (SSE-S3)
info "Enabling default encryption (AES-256)"
run aws s3api put-bucket-encryption \
    --bucket "$BUCKET" \
    --server-side-encryption-configuration \
        '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"},"BucketKeyEnabled":true}]}'

# ============================================================
# 2. Create IAM user and policy
# ============================================================
info "Creating IAM user: $IAM_USER"

if aws iam get-user --user-name "$IAM_USER" &>/dev/null; then
    warn "IAM user '$IAM_USER' already exists, skipping creation."
else
    run aws iam create-user --user-name "$IAM_USER"
fi

POLICY_NAME="stratosync-s3-${BUCKET}"
POLICY_ARN=""

# Create a scoped IAM policy for this bucket
info "Creating IAM policy: $POLICY_NAME"
POLICY_DOC=$(cat <<ENDPOLICY
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "ListBucket",
            "Effect": "Allow",
            "Action": [
                "s3:ListBucket",
                "s3:GetBucketLocation",
                "s3:GetBucketVersioning"
            ],
            "Resource": "arn:aws:s3:::${BUCKET}"
        },
        {
            "Sid": "ObjectAccess",
            "Effect": "Allow",
            "Action": [
                "s3:GetObject",
                "s3:GetObjectVersion",
                "s3:PutObject",
                "s3:DeleteObject",
                "s3:ListMultipartUploadParts",
                "s3:AbortMultipartUpload"
            ],
            "Resource": "arn:aws:s3:::${BUCKET}/*"
        }
    ]
}
ENDPOLICY
)

if ! $DRY_RUN; then
    # Check if policy already exists
    EXISTING_ARN=$(aws iam list-policies --scope Local --query "Policies[?PolicyName=='${POLICY_NAME}'].Arn" --output text 2>/dev/null || true)
    if [[ -n "$EXISTING_ARN" && "$EXISTING_ARN" != "None" ]]; then
        warn "Policy '$POLICY_NAME' already exists ($EXISTING_ARN), skipping creation."
        POLICY_ARN="$EXISTING_ARN"
    else
        POLICY_ARN=$(aws iam create-policy \
            --policy-name "$POLICY_NAME" \
            --policy-document "$POLICY_DOC" \
            --query 'Policy.Arn' --output text)
        info "Created policy: $POLICY_ARN"
    fi

    # Attach policy to user
    info "Attaching policy to user"
    aws iam attach-user-policy \
        --user-name "$IAM_USER" \
        --policy-arn "$POLICY_ARN"
else
    echo "[dry-run] aws iam create-policy --policy-name $POLICY_NAME --policy-document <scoped-policy>"
    echo "[dry-run] aws iam attach-user-policy --user-name $IAM_USER --policy-arn <policy-arn>"
fi

# ============================================================
# 3. Create access keys
# ============================================================
info "Creating access keys for $IAM_USER"

ACCESS_KEY_ID=""
SECRET_ACCESS_KEY=""

if ! $DRY_RUN; then
    KEY_OUTPUT=$(aws iam create-access-key --user-name "$IAM_USER" --output json)
    ACCESS_KEY_ID=$(echo "$KEY_OUTPUT" | jq -r '.AccessKey.AccessKeyId')
    SECRET_ACCESS_KEY=$(echo "$KEY_OUTPUT" | jq -r '.AccessKey.SecretAccessKey')

    echo ""
    echo "  Access Key ID:     $ACCESS_KEY_ID"
    echo "  Secret Access Key: $SECRET_ACCESS_KEY"
    echo ""
    warn "Save the secret key now — it cannot be retrieved again."
    echo ""
else
    ACCESS_KEY_ID="<access-key-id>"
    SECRET_ACCESS_KEY="<secret-access-key>"
    echo "[dry-run] aws iam create-access-key --user-name $IAM_USER"
fi

# ============================================================
# 4. Configure rclone remote
# ============================================================
info "Configuring rclone remote: $RCLONE_REMOTE"

if rclone listremotes | grep -q "^${RCLONE_REMOTE}:$"; then
    warn "rclone remote '$RCLONE_REMOTE' already exists."
    read -rp "Overwrite? [y/N] " overwrite
    if [[ "$overwrite" =~ ^[Yy]$ ]]; then
        run rclone config delete "$RCLONE_REMOTE"
    else
        info "Keeping existing rclone remote."
    fi
fi

if ! rclone listremotes | grep -q "^${RCLONE_REMOTE}:$"; then
    run rclone config create "$RCLONE_REMOTE" s3 \
        provider AWS \
        access_key_id "$ACCESS_KEY_ID" \
        secret_access_key "$SECRET_ACCESS_KEY" \
        region "$REGION" \
        acl private \
        server_side_encryption AES256 \
        no_check_bucket true
fi

# Verify connectivity
if ! $DRY_RUN; then
    info "Verifying rclone can reach the bucket..."
    if rclone lsd "${RCLONE_REMOTE}:${BUCKET}/" &>/dev/null; then
        info "rclone connectivity verified."
    else
        # lsd returns empty on an empty bucket but exits 0; a real
        # error (permissions, region mismatch) exits non-zero.
        # Try ls as a fallback — empty bucket returns 0 with no output.
        if rclone ls "${RCLONE_REMOTE}:${BUCKET}/" &>/dev/null; then
            info "rclone connectivity verified (empty bucket)."
        else
            warn "Could not verify rclone connectivity. Check credentials and region."
        fi
    fi
fi

# ============================================================
# 5. Add mount to stratosync config
# ============================================================
info "Adding mount to stratosync config: $STRATOSYNC_CONFIG"

MOUNT_BLOCK=$(cat <<ENDMOUNT

[[mount]]
name          = "${RCLONE_REMOTE}"
remote        = "${RCLONE_REMOTE}:${BUCKET}/"
mount_path    = "${MOUNT_PATH}"
cache_quota   = "${CACHE_QUOTA}"
poll_interval = "${POLL_INTERVAL}"
enabled       = true

[mount.rclone]
extra_flags = ["--s3-acl", "private"]
transfers   = 4
checkers    = 8
ENDMOUNT
)

if ! $DRY_RUN; then
    CONFIG_DIR=$(dirname "$STRATOSYNC_CONFIG")
    mkdir -p "$CONFIG_DIR"

    if [[ -f "$STRATOSYNC_CONFIG" ]]; then
        if grep -q "name.*=.*\"${RCLONE_REMOTE}\"" "$STRATOSYNC_CONFIG" 2>/dev/null; then
            warn "A mount named '${RCLONE_REMOTE}' already exists in config. Skipping."
        else
            echo "$MOUNT_BLOCK" >> "$STRATOSYNC_CONFIG"
            info "Appended mount to $STRATOSYNC_CONFIG"
        fi
    else
        cat > "$STRATOSYNC_CONFIG" <<ENDCFG
[daemon]
log_level = "info"
${MOUNT_BLOCK}
ENDCFG
        info "Created $STRATOSYNC_CONFIG"
    fi
else
    echo "[dry-run] Would append to $STRATOSYNC_CONFIG:"
    echo "$MOUNT_BLOCK"
fi

# Create mount point directory
run mkdir -p "$MOUNT_PATH"

# ============================================================
# Done
# ============================================================
echo ""
info "Setup complete!"
echo ""
echo "  Bucket:        s3://$BUCKET (region: $REGION, versioned, encrypted)"
echo "  IAM User:      $IAM_USER (scoped to this bucket only)"
echo "  rclone Remote: $RCLONE_REMOTE"
echo "  Mount Path:    $MOUNT_PATH"
echo "  Config:        $STRATOSYNC_CONFIG"
echo ""
echo "Next steps:"
echo "  1. Start the daemon:  RUST_LOG=stratosync=debug cargo run -p stratosync-daemon"
echo "  2. Check status:      cargo run -p stratosync-cli -- status"
echo "  3. Browse files:      ls $MOUNT_PATH"
echo ""
echo "Note: S3 does not support change-token delta polling."
echo "      Stratosync will use full directory listing every $POLL_INTERVAL."
echo "      Increase poll_interval for large buckets to reduce API costs."
