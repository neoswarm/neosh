/**
 * `git status`, shared.
 *
 * Three parts of this plugin want a checkout's status at the same moments — the badge on its row,
 * the pull request matched to its branch, and the footer's branch — and the moment they all want
 * it is a turn ending. Asked separately that was three `git status` per project, one after another,
 * every time an answer arrived; on a large tree each is a fraction of a second of a subprocess.
 * Here, a directory has at most one read in flight, and a read younger than the caller can bear is
 * answered from what came back.
 */
import type { Neosh, RepoStatus } from "@neosh/api";

/** How old an answer a caller reacting to the same event as another may be handed. */
export const SHARED_MS = 1000;

const recent = new Map<string, { at: number; status: RepoStatus | null }>();
const flying = new Map<string, Promise<RepoStatus | null>>();

/**
 * The status of `cwd`, read at most once at a time and reused when younger than `maxAge`.
 *
 * `null` when it is not a repository or the read failed, as every caller here already treated a
 * rejection.
 */
export function statusOf(neosh: Neosh, cwd: string, maxAge = SHARED_MS): Promise<RepoStatus | null> {
  // Zero is "after something changed it": a read already in flight may have started before the
  // change, so it is not one to join.
  if (maxAge > 0) {
    const last = recent.get(cwd);
    if (last && Date.now() - last.at < maxAge) return Promise.resolve(last.status);
    const already = flying.get(cwd);
    if (already) return already;
  }
  const read = neosh.git.status({ cwd }).catch(() => null).then((status) => {
    recent.set(cwd, { at: Date.now(), status });
    if (flying.get(cwd) === read) flying.delete(cwd);
    return status;
  });
  flying.set(cwd, read);
  return read;
}

/** A status somebody already has — a fetch answers with one — so the next reader need not ask. */
export function noteStatus(cwd: string, status: RepoStatus | null): void {
  recent.set(cwd, { at: Date.now(), status });
}

/**
 * Forget what was read, after something may have changed it — and any read in flight with it,
 * since that one may have started before the change.
 */
export function forgetStatus(cwd?: string): void {
  if (cwd === undefined) {
    recent.clear();
    flying.clear();
  } else {
    recent.delete(cwd);
    flying.delete(cwd);
  }
}

/**
 * Forget everything when a turn ends, before anybody asks.
 *
 * Registered ahead of every other listener in the plugin, because a turn ending is exactly when
 * the tree has changed and everybody asks at once: the first of them reads, the rest share it, and
 * nobody is handed the read the five-second refresh made a moment before the agent's last write.
 */
export function forgetOnTurnEnd(neosh: Neosh): { dispose(): void } {
  return neosh.agent.onTurnEnd(() => forgetStatus());
}
