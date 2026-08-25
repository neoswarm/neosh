/**
 * There is a newer neosh, and here is what pressing this would do.
 *
 * Two things, and the second is the one that needs care. Saying *there is an update* is news you did
 * not ask for and cannot otherwise see, which is the definition of an alert — so it is raised once,
 * and never again for the same version. Saying *what updating means here* is not one sentence: a
 * binary Homebrew owns is updated by `brew`, one inside `node_modules` by npm, and only a standalone
 * one is neosh's to replace. The host works out which; this draws the answer.
 *
 * The row lives under the plan strip, because both are facts about the workspace rather than about
 * a conversation, and the plan is the thing you look at when you look down there. It is only ever
 * present when there is something to say — an update waiting, or a restart owed — so the column's
 * job stays your conversations.
 */
import type { PluginContext, UpdateStatus } from "@neosh/api";

const NS = "update";
/** The section id, so the row can be taken off again by name. */
const ROW = "update";
/** How often to ask, once the workspace is up. A day: the thing being watched moves in weeks. */
const EVERY_MS = 6 * 60 * 60 * 1000;

export async function activate({ neosh, subscriptions }: PluginContext) {
  /** The version we have already interrupted somebody about. */
  let announced: string | null = null;
  /** What is drawn, so a tick that changes nothing is not a round trip. */
  let drawn: string | null = null;

  /** What the row says, given what the host worked out. `null` when there is nothing to say. */
  const rowFor = (s: UpdateStatus) => {
    if (s.restart_pending) {
      return {
        text: `restart for ${s.latest ?? "the update"}`,
        hl: "Status.Pending",
        right: { text: "↵", hl: "Comment" },
        command: `${NS}.restart`,
      };
    }
    if (!s.behind || !s.latest) return null;
    // What the key would *do* is on the row, because a key whose effect you cannot predict from
    // the row it is pointed at is one people learn not to press.
    return {
      text: `neosh ${s.latest} available`,
      hl: "Status.Unread",
      right: { text: "↵", hl: "Comment" },
      command: `${NS}.apply`,
    };
  };

  const refresh = async (force = false) => {
    const status = await neosh.update.check(force).catch(() => null);
    if (!status) return;

    const row = rowFor(status);
    const key = JSON.stringify(row);
    if (key !== drawn) {
      drawn = key;
      if (row) {
        // Under the plan rather than above it: the plan is the thing somebody came to look at, and
        // a row that pushes it down every time a release happens is a row that moved their gauge.
        await neosh.ext.contribute("sidebar.section", ROW, { after: "plan", rows: [row] }, {
          priority: -9,
        }).catch(() => {});
      } else {
        await neosh.ext.remove("sidebar.section", ROW).catch(() => {});
      }
    }

    // Once per version. A notification that repeats is one people turn off, and this one has to
    // survive being ignored for a week.
    if (status.behind && status.latest && status.latest !== announced) {
      announced = status.latest;
      await neosh.alert(
        `neosh ${status.latest}`,
        status.self_updatable
          ? "A new version is out. Update from the sidebar, or run update.apply."
          : `A new version is out. Update with: ${status.upgrade_command ?? "your package manager"}`,
        { level: "info" },
      ).catch(() => {});
    }
  };

  subscriptions.push(
    await neosh.cmd.register(`${NS}.check`, async () => {
      await refresh(true);
      const s = await neosh.update.check().catch(() => null);
      if (!s) return neosh.notify("Could not check for updates", "warn");
      if (s.error) return neosh.notify(`Update check failed: ${s.error}`, "warn");
      if (!s.behind) return neosh.notify(`neosh ${s.current} is the newest`, "info");
      return neosh.notify(`neosh ${s.latest} is available`, "info");
    }, { desc: "Check for a newer neosh" }),
  );

  subscriptions.push(
    await neosh.cmd.register(`${NS}.apply`, async () => {
      neosh.progress(`${NS}`, "Updating neosh…");
      const outcome = await neosh.update.apply().catch((e) => ({
        outcome: "failed" as const,
        reason: String(e),
      }));
      switch (outcome.outcome) {
        case "up_to_date":
          return neosh.notify(`neosh ${outcome.version} is the newest`, "info");
        case "applied":
          await refresh(true);
          // Asked, never done. A restart ends turns in conversations this terminal cannot see, so
          // the decision belongs to a person who has been told that.
          return neosh.notify(
            `neosh ${outcome.version} is installed — restart to use it`,
            "info",
          );
        case "delegated":
          // Not run for them. A keypress that drives somebody's package manager is how a machine
          // ends up in a state its owner cannot explain.
          return neosh.notify(`Update with: ${outcome.command}`, "info");
        default:
          return neosh.notify(`Update failed: ${outcome.reason}`, "error");
      }
    }, { desc: "Update neosh to the newest version" }),
  );

  subscriptions.push(
    await neosh.cmd.register(`${NS}.restart`, async () => {
      try {
        await neosh.update.restart();
      } catch (e) {
        // The host refuses while any turn is in flight, and names them. Passed through as-is:
        // "something is running" sends somebody hunting for which conversation.
        return neosh.notify(String(e), "warn");
      }
    }, { desc: "Restart the workspace to finish an update" }),
  );

  // No `sidebar.action` of its own, deliberately. A contributed row already runs its `command` on
  // `↵`, and one row that means the one thing there is to do — update, or restart once it is
  // downloaded — is better than two keys for two states that never coexist.
  //
  // It also sidesteps something worth fixing properly one day: `on: "custom"` matches *any*
  // contributed row rather than the contributor's own, so a key added here is advertised on the
  // plan strip too, and the strip's own `<Tab>` hint gets pushed off the end of the row.

  await refresh();
  const timer = await neosh.timer.every(EVERY_MS, () => void refresh(true));
  subscriptions.push(timer);
  subscriptions.push({
    dispose: () => void neosh.ext.remove("sidebar.section", ROW).catch(() => {}),
  });
}
