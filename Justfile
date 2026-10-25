set windows-shell := ["bash", "-c"]

NAME := "myplay.exe"
SRC  := "./target/release/myplay.exe"
DST  := "D:/软件/" + NAME

# 发布用：git remote 名与发布分支（本仓库 remote 名为 github，分支为 master）
GH_REMOTE := "github"
GH_BRANCH := "master"


@dev:
  cargo run -p myplay

@test:
  cargo test -p myplay

@build:
  cargo build -r -p myplay

install:
	#!/usr/bin/env bash
	set -euo pipefail

	NAME="{{NAME}}"
	SRC="{{SRC}}"
	DST="{{DST}}"

	was_running=0

	echo "==> 检查 $NAME 是否正在运行..."
	if tasklist //FI "IMAGENAME eq $NAME" 2>/dev/null | grep -qi "$NAME"; then
		was_running=1
		echo "==> 正在运行，尝试停止..."
		taskkill //F //IM "$NAME" >/dev/null 2>&1 || true

		# 等待进程真正退出（最多约 3 秒）
		for _ in {1..10}; do
			tasklist //FI "IMAGENAME eq $NAME" 2>/dev/null | grep -qi "$NAME" || break
			sleep 0.3
		done
	else
		echo "==> 未运行，跳过停止/启动步骤"
	fi

	echo "==> 复制文件 $SRC -> $DST ..."
	if ! cp -f "$SRC" "$DST"; then
		echo "❌ 复制失败，目标可能仍被占用" >&2
		exit 1
	fi

	if [[ $was_running -eq 1 ]]; then
		echo "==> 重新启动程序..."
		cmd //c start "" "$(cygpath -w "$DST")"
	else
		echo "==> 未运行，不启动"
	fi

	echo "==> 完成"

# ---- GitHub 发布 ----

# 发布细节：
# - 版本号只从 crates/myplay/Cargo.toml 的 [package] 段读取，不用传参：先改好 version；
# - 更新说明只认 CHANGELOG.md 里同版本段落，缺失或为空即中止；
# - 校验（版本号 / CHANGELOG / tag / 已有 Release）全部通过后打印摘要，回车确认才开始动文件。
# 一条命令发布到 GitHub Releases：just release
release:
	#!/usr/bin/env bash
	set -euo pipefail
	cd "$(git rev-parse --show-toplevel)"

	REMOTE="{{GH_REMOTE}}"
	BRANCH="{{GH_BRANCH}}"
	MANIFEST="crates/myplay/Cargo.toml"
	CHANGELOG="CHANGELOG.md"
	ASSET_DIR="target/dist"

	die() { echo "❌ $*" >&2; exit 1; }

	# 1) 环境校验：gh 可用且已登录、远端存在、在发布分支
	command -v gh >/dev/null 2>&1 || die "未找到 gh，先安装 GitHub CLI"
	gh auth status >/dev/null 2>&1 || die "gh 未登录，先执行 gh auth login"
	git remote get-url "$REMOTE" >/dev/null 2>&1 || die "缺少 git remote：$REMOTE"
	[[ "$(git rev-parse --abbrev-ref HEAD)" == "$BRANCH" ]] || die "请先切到 $BRANCH 分支再发布"
	[[ -f "$MANIFEST" ]] || die "缺少 $MANIFEST"

	# 2) 版本号只来自应用清单的 [package] 段
	VERSION=$(awk '
		/^\[package\]/ { in_pkg = 1; next }
		in_pkg && /^\[/ { exit }
		in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
			sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "")
			sub(/".*$/, "")
			print
			exit
		}
	' "$MANIFEST")
	[[ -n "$VERSION" ]] || die "没能从 $MANIFEST 的 [package] 段读到 version"
	[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "版本号格式应为 MAJOR.MINOR.PATCH：$VERSION"

	TAG="v${VERSION}"
	ASSET="${ASSET_DIR}/myplay-${VERSION}-windows-x64.zip"
	NOTES="${ASSET_DIR}/release-notes-${VERSION}.md"

	# 说明文件在脚本退出时统一删除（成功失败都清）
	trap 'rm -f "$NOTES"' EXIT

	# 3) 更新内容必填：取 CHANGELOG.md 中该版本段落，缺失或空段落直接中止
	[[ -f "$CHANGELOG" ]] || die "缺少 $CHANGELOG"
	mkdir -p "$ASSET_DIR"
	awk -v ver="$VERSION" '
		index($0, "## [" ver "]") == 1 { if (found) exit; found = 1; next }
		found && substr($0, 1, 4) == "## [" { exit }
		found { print }
	' "$CHANGELOG" > "$NOTES"
	sed -i '/./,$!d' "$NOTES"
	grep -q '[^[:space:]]' "$NOTES" || die "$CHANGELOG 里没有 [$VERSION] 段落的更新内容，先写好再发布"

	# 4) 版本号冲突检查：本地 tag、远端 tag、GitHub 上是否已有同名 Release
	if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
		die "本地已存在 tag $TAG（改 $MANIFEST 里的版本号，或先删掉旧 tag）"
	fi
	if git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$TAG" >/dev/null 2>&1; then
		die "远端已存在 tag $TAG（改 $MANIFEST 里的版本号，或先删掉旧 Release）"
	fi
	if gh release view "$TAG" >/dev/null 2>&1; then
		die "GitHub 上已存在 Release $TAG"
	fi

	# 5) 摘要 + 回车确认：在此之前不构建、不改文件、不推送
	echo
	echo "==> 版本    $VERSION  （读自 $MANIFEST）"
	echo "==> tag     $TAG  →  $REMOTE / $BRANCH"
	echo "==> 产物    $ASSET"
	echo "==> 提交    $MANIFEST、Cargo.lock、$CHANGELOG"
	if git status --porcelain | grep -qvE "^.. (crates/myplay/Cargo\.toml|Cargo\.lock|CHANGELOG\.md)$"; then
		echo "==> 注意    工作区还有其它未提交改动，它们不会被提交"
	fi
	echo "==> Release 说明（取自 $CHANGELOG）"
	sed -e 's/^/    /' "$NOTES"
	echo
	read -r -p "回车键继续发布，Ctrl-C 取消 > " _ || die "已取消"
	echo

	# 6) release 构建（Cargo.lock 里的应用版本号随之更新）
	echo "==> 构建 release ..."
	cargo build -r -p myplay
	[[ -f target/release/myplay.exe ]] || die "构建产物缺失：target/release/myplay.exe"

	# 7) 打包（只含 exe，文件名带版本与平台）
	echo "==> 打包 $ASSET"
	rm -f "$ASSET"
	WIN_ASSET=$(cygpath -w "$ASSET" 2>/dev/null || printf '%s' "$ASSET" | sed 's#/#\\\\#g')
	powershell -NoProfile -Command "Compress-Archive -Path 'target/release/myplay.exe' -DestinationPath '$WIN_ASSET' -Force" \
		|| die "打包失败"
	[[ -f "$ASSET" ]] || die "打包产物缺失：$ASSET"

	# 8) 提交版本号与更新记录，打 tag 并推送
	git add "$MANIFEST" Cargo.lock "$CHANGELOG"
	if git diff --cached --quiet; then
		echo "==> 版本号与更新记录无变化，跳过提交"
	else
		git commit -m "chore(release): $TAG"
	fi
	git tag -a "$TAG" -m "$TAG"
	git push "$REMOTE" "$BRANCH"
	git push "$REMOTE" "$TAG"

	# 9) 上传产物（说明取自已提取的 CHANGELOG 段落）
	echo "==> 创建 GitHub Release $TAG"
	gh release create "$TAG" "$ASSET" --title "$TAG" --notes-file "$NOTES"

	echo "==> 完成：$(gh release view "$TAG" --json url -q .url)"
