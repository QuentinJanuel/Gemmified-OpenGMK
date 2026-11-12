// Deletes the builds a release superseded: every object under a <major>.<minor>.<patch>/ prefix of
// the bucket except the new version's. Run by the release step AFTER the feed is published.
//
//   node scripts/prune-old-versions.mjs <bucket> <new version> [--dry-run]
//
// Anything else is kept: latest.json, the new version, and any key that is not under a plain
// semver prefix (prereleases like 1.0.0-rc.1/ included). wrangler cannot list a bucket, so the keys
// are listed through Cloudflare's REST API with the same CLOUDFLARE_API_TOKEN / CLOUDFLARE_ACCOUNT_ID
// that wrangler uses; each delete is `wrangler r2 object delete`, as the uploads are `put`.
// --dry-run prints the keys and deletes nothing.
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

/** The keys to delete: under a <major>.<minor>.<patch>/ prefix, and not under `version`/. */
export function oldVersionKeys(keys, version) {
  return keys.filter((key) => /^\d+\.\d+\.\d+\//.test(key) && !key.startsWith(`${version}/`));
}

async function listKeys(bucket) {
  const { CLOUDFLARE_API_TOKEN: token, CLOUDFLARE_ACCOUNT_ID: account } = process.env;
  if (!token || !account) throw new Error("CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID are required");
  const keys = [];
  let cursor;
  do {
    const query = new URLSearchParams({ per_page: "1000", ...(cursor && { cursor }) });
    const response = await fetch(
      `https://api.cloudflare.com/client/v4/accounts/${account}/r2/buckets/${bucket}/objects?${query}`,
      { headers: { Authorization: `Bearer ${token}` } },
    );
    const body = await response.json();
    if (!response.ok || !body.success) throw new Error(`list ${bucket}: HTTP ${response.status}`);
    keys.push(...body.result.map((object) => object.key));
    cursor = body.result_info?.is_truncated ? body.result_info.cursor : undefined;
  } while (cursor);
  return keys;
}

async function main() {
  const args = process.argv.slice(2);
  const dryRun = args.includes("--dry-run");
  const [bucket, version] = args.filter((a) => a !== "--dry-run");
  if (!version) throw new Error("usage: prune-old-versions.mjs <bucket> <new version> [--dry-run]");
  if (!/^\d+\.\d+\.\d+/.test(version)) throw new Error(`not a version: ${version}`);

  const old = oldVersionKeys(await listKeys(bucket), version);
  console.log(old.length ? `deleting:\n${old.join("\n")}` : "no old builds to delete");
  if (dryRun) return;
  for (const key of old)
    execFileSync("pnpm", ["dlx", "wrangler@4.120.0", "r2", "object", "delete", `${bucket}/${key}`, "--remote"], {
      stdio: "inherit",
    });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) await main();
