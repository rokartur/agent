export const meta = {
  name: 'deep-research',
  description: 'Research a question across many web sources, cross-check every claim, and return a cited report',
  phases: ['Plan', 'Search', 'Verify', 'Report'],
}

const question = typeof args === 'string' ? args : args && args.question
if (!question) throw new Error('deep-research needs a question: pass it as args, for example /deep-research <question>.')

phase('Plan')
const plan = await agent(
  `Plan web research for this question: ${question}\n\nList 4 to 6 distinct angles that together answer it, each with 2 or 3 search queries. Do not search yet.`,
  {
    label: 'plan angles',
    explore: true,
    schema: {
      type: 'object',
      required: ['angles'],
      properties: {
        angles: {
          type: 'array',
          minItems: 1,
          maxItems: 6,
          items: {
            type: 'object',
            required: ['angle', 'queries'],
            properties: { angle: { type: 'string' }, queries: { type: 'array', items: { type: 'string' } } },
          },
        },
      },
    },
  },
)

phase('Search')
const claimSchema = {
  type: 'object',
  required: ['claims'],
  properties: {
    claims: {
      type: 'array',
      items: {
        type: 'object',
        required: ['claim', 'sources'],
        properties: { claim: { type: 'string' }, sources: { type: 'array', items: { type: 'string' } } },
      },
    },
  },
}
const found = await pipeline(plan.angles, (angle) =>
  agent(
    `Research one angle of the question "${question}".\n\nAngle: ${angle.angle}\nStart from these queries: ${angle.queries.join(' | ')}\n\n` +
      'Use WebSearch, then WebFetch the most authoritative pages (primary sources, official docs, changelogs). ' +
      'Return the specific factual claims that answer the question, each with the URLs that state it.',
    { label: angle.angle, schema: claimSchema },
  ),
)
const claims = found.filter(Boolean).flatMap((result) => result.claims)
log(`${claims.length} claims from ${found.filter(Boolean).length} angles`)

phase('Verify')
const verdictSchema = {
  type: 'object',
  required: ['verdict', 'note'],
  properties: { verdict: { enum: ['supported', 'refuted', 'unverified'] }, note: { type: 'string' } },
}
const checked = await pipeline(claims, (claim) =>
  parallel(
    [0, 1].map(() => () =>
      agent(
        `Check this claim independently: "${claim.claim}"\nIt is said to come from: ${claim.sources.join(', ')}\n\n` +
          'Fetch the sources and search for others. Answer supported only when a reliable source states it, refuted when reliable sources contradict it, ' +
          'and unverified when you could not check it (a page would not load, a rate limit, no source found).',
        { label: `verify: ${claim.claim.slice(0, 50)}`, schema: verdictSchema },
      ),
    ),
  ).then((votes) => ({ ...claim, votes: votes.filter(Boolean) })),
)
const verified = checked.filter(Boolean)
const kept = verified.filter((claim) => !claim.votes.some((vote) => vote.verdict === 'refuted'))
const supported = kept.filter((claim) => claim.votes.length > 0 && claim.votes.every((vote) => vote.verdict === 'supported'))
const unverified = kept.filter((claim) => !supported.includes(claim))
log(`${supported.length} supported, ${unverified.length} unverified, ${verified.length - kept.length} refuted and dropped`)

phase('Report')
return await agent(
  `Write a cited research report answering: ${question}\n\n` +
    `Supported claims, with their sources:\n${JSON.stringify(supported.map(({ claim, sources }) => ({ claim, sources })), null, 1)}\n\n` +
    `Claims the verifiers could not check:\n${JSON.stringify(unverified.map(({ claim, sources }) => ({ claim, sources })), null, 1)}\n\n` +
    'Use only these claims. Cite each with its URLs. List the unverified ones in their own section, marked as unverified. Do not search again.',
  { label: 'write report', explore: true },
)
