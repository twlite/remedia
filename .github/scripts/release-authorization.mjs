/** Require current repository write access for both the original and rerun actors. */
export async function authorizeRelease({ env = process.env, request = fetch } = {}) {
  if (
    !['push', 'workflow_dispatch'].includes(env.GITHUB_EVENT_NAME) ||
    env.GITHUB_REF !== 'refs/heads/main'
  ) {
    throw new Error('Releases require a push or workflow dispatch on main.');
  }
  const {
    GITHUB_REPOSITORY: repository,
    GITHUB_ACTOR: actor,
    GITHUB_TRIGGERING_ACTOR: triggeringActor,
    GH_TOKEN: token,
  } = env;
  if (
    !repository ||
    !/^[^/\s]+\/[^/\s]+$/.test(repository) ||
    !actor ||
    !triggeringActor ||
    !token
  ) {
    throw new Error(
      'Release authorization requires repository, actor, triggering actor, and GitHub token.',
    );
  }
  for (const username of new Set([actor, triggeringActor])) {
    const response = await request(
      `${env.GITHUB_API_URL ?? 'https://api.github.com'}/repos/${repository}/collaborators/${encodeURIComponent(username)}/permission`,
      {
        headers: {
          Accept: 'application/vnd.github+json',
          Authorization: `Bearer ${token}`,
          'X-GitHub-Api-Version': '2022-11-28',
        },
        signal: AbortSignal.timeout(10_000),
        redirect: 'error',
      },
    );
    if (response.status !== 200) {
      throw new Error(
        `Cannot authorize release actor ${username}: GitHub returned HTTP ${response.status}.`,
      );
    }
    const result = await response.json();
    if (!['write', 'maintain', 'admin'].includes(result?.permission)) {
      throw new Error(
        `Release actor ${username} must have repository write, maintain, or admin access.`,
      );
    }
  }
}
