import { LatestVersionBadge, ReleaseDownload } from '@/components/release-download';

export default function HomePage() {
  return (
    <div className="mx-auto flex max-w-2xl flex-1 flex-col justify-center px-6 text-center">
      <div className="mb-4 inline-flex items-start justify-center gap-2">
        <h1 className="text-4xl font-bold leading-none">Recall</h1>
        <LatestVersionBadge />
      </div>
      <p className="mb-8 text-lg text-fd-muted-foreground">
        Local-first search across every AI coding session on your machine.
      </p>
      <ReleaseDownload />
    </div>
  );
}
