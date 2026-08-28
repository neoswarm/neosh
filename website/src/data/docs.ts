// The docs sidebar, and the order prev/next walks in. One place, so the
// sidebar, the breadcrumb and the pager can never disagree.

export interface DocLink {
  title: string;
  href: string;
}

export interface DocSection {
  title: string;
  links: DocLink[];
}

export const sections: DocSection[] = [
  {
    title: "Getting started",
    links: [
      { title: "Start Guide", href: "/docs" },
      { title: "Installation", href: "/docs/installation" },
      { title: "Concepts", href: "/docs/concepts" },
      { title: "Scripting", href: "/docs/scripting" },
    ],
  },
  {
    title: "Customization",
    links: [
      { title: "Configuration", href: "/docs/configuration" },
      { title: "Options", href: "/docs/options" },
      { title: "Keymaps", href: "/docs/keymaps" },
      { title: "Appearance", href: "/docs/appearance" },
      { title: "Sidebar and projects", href: "/docs/sidebar" },
      { title: "Models", href: "/docs/models" },
      { title: "Machines", href: "/docs/machines" },
    ],
  },
  {
    title: "Plugin development",
    links: [
      { title: "Writing a plugin", href: "/docs/plugins" },
      { title: "The plugin API", href: "/docs/plugin-api" },
      { title: "Panels and extension points", href: "/docs/panels" },
    ],
  },
  {
    title: "Reference",
    links: [
      { title: "Keys", href: "/docs/keys" },
      { title: "Docs for agents", href: "/docs/agents" },
    ],
  },
];

export const flatDocs = sections.flatMap((s) =>
  s.links.map((l) => ({ ...l, section: s.title })),
);
