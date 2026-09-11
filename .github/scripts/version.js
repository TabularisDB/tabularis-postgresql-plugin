// Shared semver classification/computation logic for this repo's release
// tooling. Used by both ci.yml's per-PR version-suggestion job and
// prepare-release.yml's multi-PR aggregation job — kept in one place so
// the two never drift.

const CHANNEL_RANK = { alpha: 1, beta: 2, rc: 3, stable: 4 };

const TITLE_PATTERN = /^([a-zA-Z]+)(\(([^)]+)\))?(!)?: (.+)$/;

// Classifies a Conventional Commits PR title (+ optional body, for a
// "BREAKING CHANGE:" footer) into a bump class. Throws if the title
// doesn't match Conventional Commits — callers decide how to surface that.
function classifyTitle(title, body) {
  const m = title.match(TITLE_PATTERN);
  if (!m) {
    throw new Error(
      `PR title does not match Conventional Commits format (type: subject): ${title}`
    );
  }
  const type = m[1];
  const bang = m[4];

  let breaking = Boolean(bang);
  if (body && /^BREAKING[ -]CHANGE:/im.test(body)) {
    breaking = true;
  }

  let cls;
  switch (type) {
    case "feat":
      cls = "minor";
      break;
    case "fix":
    case "refactor":
    case "perf":
      cls = "patch";
      break;
    default:
      cls = "none";
  }
  if (breaking) cls = "major";

  return { type, breaking, class: cls };
}

// Picks the highest-ranked prerelease:* label name (without the prefix)
// from a list of label names. Returns null if none match.
function highestChannel(labelNames) {
  let best = null;
  for (const name of labelNames) {
    if (!name.startsWith("prerelease:")) continue;
    const channel = name.slice("prerelease:".length);
    if (!(channel in CHANNEL_RANK)) continue;
    if (!best || CHANNEL_RANK[channel] > CHANNEL_RANK[best]) {
      best = channel;
    }
  }
  return best;
}

function parseVersion(v) {
  const m = v.match(/^(\d+)\.(\d+)\.(\d+)(?:-([a-zA-Z]+)\.(\d+))?$/);
  if (!m) throw new Error(`Cannot parse version: ${v}`);
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    stage: m[4] || null,
    stageNum: m[5] ? Number(m[5]) : null,
  };
}

function formatVersion(v) {
  const base = `${v.major}.${v.minor}.${v.patch}`;
  return v.stage ? `${base}-${v.stage}.${v.stageNum}` : base;
}

function bumpStable(v, cls) {
  const out = {
    major: v.major,
    minor: v.minor,
    patch: v.patch,
    stage: null,
    stageNum: null,
  };
  if (cls === "major") {
    out.major += 1;
    out.minor = 0;
    out.patch = 0;
  } else if (cls === "minor") {
    out.minor += 1;
    out.patch = 0;
  } else if (cls === "patch") {
    out.patch += 1;
  }
  return out;
}

function computeNextVersion(baselineStr, classification, channelLabel) {
  const baseline = parseVersion(baselineStr);
  if (channelLabel === "stable") {
    if (baseline.stage) {
      return formatVersion({
        major: baseline.major,
        minor: baseline.minor,
        patch: baseline.patch,
        stage: null,
        stageNum: null,
      });
    }
    return formatVersion(bumpStable(baseline, classification));
  }
  if (baseline.stage === channelLabel) {
    return formatVersion({ ...baseline, stageNum: baseline.stageNum + 1 });
  }
  let base = { major: baseline.major, minor: baseline.minor, patch: baseline.patch };
  if (!baseline.stage) {
    const bumped = bumpStable(baseline, classification);
    base = { major: bumped.major, minor: bumped.minor, patch: bumped.patch };
  }
  return formatVersion({ ...base, stage: channelLabel, stageNum: 1 });
}

module.exports = {
  CHANNEL_RANK,
  classifyTitle,
  highestChannel,
  parseVersion,
  formatVersion,
  bumpStable,
  computeNextVersion,
};
