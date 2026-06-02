import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

const config = {
  basePath: '/Recall',
  output: 'export',
  reactStrictMode: true,
  trailingSlash: true,
  images: {
    unoptimized: true,
  },
};

export default withMDX(config);
