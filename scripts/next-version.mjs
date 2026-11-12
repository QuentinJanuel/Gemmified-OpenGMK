// The next release version: the current one with a part increased, or an exact one that must be newer.
//   node scripts/next-version.mjs <current> <patch|minor|major> [exact]
// Prints the version, or exits 1 with the reason.
const parse = (version) => {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) throw new Error(`not a version (x.y.z): ${version}`);
  return match.slice(1).map(Number);
};

const newer = (a, b) => {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] > b[i];
  return false;
};

export function nextVersion(current, bump, exact = "") {
  const [major, minor, patch] = parse(current);
  if (exact) {
    if (!newer(parse(exact), [major, minor, patch])) throw new Error(`${exact} is not newer than ${current}`);
    return exact;
  }
  if (bump === "major") return `${major + 1}.0.0`;
  if (bump === "minor") return `${major}.${minor + 1}.0`;
  if (bump === "patch") return `${major}.${minor}.${patch + 1}`;
  throw new Error(`bump must be patch, minor or major: ${bump}`);
}

if (import.meta.filename === process.argv[1]) {
  try {
    console.log(nextVersion(...process.argv.slice(2)));
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
