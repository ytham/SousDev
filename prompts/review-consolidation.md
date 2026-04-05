You are consolidating PR reviews from {{review_count}} AI models into a
single coherent review comment.

## PR: {{pr_title}} (#{{pr_number}})

The following independent reviews were produced by different AI models.
Each model reviewed the same PR independently. Your job is to merge
them into a single, well-organized review.

## Guidelines

1. **Deduplicate** — if multiple models flagged the same issue, mention it
   once and note the consensus (e.g., "All models flagged this" or
   "2/3 models identified this")
2. **Preserve unique findings** — if only one model caught something,
   include it but note it was a single-model finding
3. **Prioritize by severity** — bugs and security issues first, then
   performance, then code quality and style
4. **Resolve conflicts** — if models disagree on whether something is an
   issue, present both perspectives briefly
5. **Preserve actionable inline comments** — keep the best INLINE_COMMENT
   blocks from any model, deduplicating those that reference the same
   file and line
6. **Note what looks good** — if models agree the code is well-written
   in specific areas, mention that too

## Reviews

{{reviews}}

## Output format

Write a consolidated review using this exact structure:

SUMMARY
### Consensus findings
<issues identified by 2+ models — these carry the highest confidence>

### Additional findings
<issues identified by only 1 model — still worth reviewing>

### What looks good
<areas where models agreed the code is solid>

**Models used:** {{model_names}}
END_SUMMARY

For any inline findings worth preserving (deduplicated):

INLINE_COMMENT <path>:<line>
<consolidated comment text>
END_INLINE_COMMENT
