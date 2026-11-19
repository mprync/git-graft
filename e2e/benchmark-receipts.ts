/** Publish only documented, non-sensitive object-bootstrap counters and timings. */
export function objectBootstrap(value: unknown, includeRevisions = true): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return {};
  const input = value as Record<string, unknown>;
  const safe: Record<string, unknown> = {};
  const enums: Record<string,string[]> = {
    method:['clonefile','reflink','local-pack','none'],
    source:['primary-pack','cache','local-export','ancestor-cache','none'],
    object_format:['sha1','sha256'],
  };
  for (const [key,allowed] of Object.entries(enums)) {
    const field = input[key];
    if (typeof field === 'string' && allowed.includes(field)) safe[key] = field;
  }
  for (const key of ['bytes','pack_bytes','index_bytes','shared_bytes','cache_bytes','pack_count','new_pack_count','reused_primary_packs','reused_primary_bytes']) {
    const field = input[key];
    if (typeof field === 'number' && Number.isSafeInteger(field) && field >= 0) safe[key] = field;
  }
  if (includeRevisions && typeof input.ancestor === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(input.ancestor)) safe.ancestor = input.ancestor;
  const rawTimings = input.timings;
  if (rawTimings && typeof rawTimings === 'object' && !Array.isArray(rawTimings)) {
    const source = rawTimings as Record<string,unknown>;
    const timings: Record<string,number|null> = {};
    for (const key of ['cache_seconds','ancestor_seconds','cache_publish_seconds','reachability_seconds','transfer_seconds','verify_seconds','total_seconds']) {
      const field = source[key];
      if (field === null || (typeof field === 'number' && Number.isFinite(field) && field >= 0)) timings[key] = field;
    }
    if (Object.keys(timings).length) safe.timings = timings;
  }
  return safe;
}
