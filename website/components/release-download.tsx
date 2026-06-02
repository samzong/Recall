'use client';

import { Download } from 'lucide-react';
import { useEffect, useState } from 'react';

type GitHubRelease = {
  tag_name: string;
};

const latestReleaseApiUrl = 'https://api.github.com/repos/samzong/Recall/releases/latest';
const latestReleaseUrl = 'https://github.com/samzong/Recall/releases/latest';

function useLatestVersionLabel() {
  const [release, setRelease] = useState<GitHubRelease | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    const controller = new AbortController();

    async function loadRelease() {
      try {
        const response = await fetch(latestReleaseApiUrl, {
          signal: controller.signal,
        });
        if (!response.ok) {
          setFailed(true);
          return;
        }
        const data = (await response.json()) as GitHubRelease;
        setRelease(data);
      } catch (error) {
        if (!controller.signal.aborted) {
          setFailed(true);
        }
      }
    }

    void loadRelease();

    return () => controller.abort();
  }, []);

  return release?.tag_name ?? (failed ? 'latest' : 'latest');
}

export function LatestVersionBadge() {
  const versionLabel = useLatestVersionLabel();

  return (
    <span className="mt-0.5 rounded-full border bg-fd-card px-2 py-0.5 text-xs font-medium leading-none text-fd-muted-foreground">
      {versionLabel}
    </span>
  );
}

export function ReleaseDownload() {
  return (
    <div className="mx-auto flex w-full max-w-md flex-col items-center gap-3">
      <code className="rounded-md border bg-fd-card px-3 py-1.5 text-sm text-fd-foreground">
        brew install samzong/tap/recall
      </code>
      <a
        href={latestReleaseUrl}
        className="inline-flex items-center gap-3 rounded-md bg-fd-primary px-5 py-3 text-left text-fd-primary-foreground transition-colors hover:bg-fd-primary/90"
      >
        <Download className="size-4" aria-hidden="true" />
        <span className="text-sm font-medium leading-none">Download</span>
      </a>
    </div>
  );
}
