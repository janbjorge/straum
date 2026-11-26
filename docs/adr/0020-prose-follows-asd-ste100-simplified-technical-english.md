# ADR-0020: Prose follows ASD-STE100 Simplified Technical English

Status: Accepted 2026-11-26

## Context

Straum has prose in many places: the guide, the ADRs, comments, error
text, log lines and commit messages. Agents write most of it, and
people read it. Some readers do not have English as a first language.
Some readers use a machine translation. Long sentences, the passive
voice and synonyms make a reader guess the meaning.

The glossary gives one name to each Straum thing, but general words
had no rule. AGENTS.md added the ASD-STE100 rule on 2026-11-25, but no
record held the decision (ADR-0001).

ASD-STE100 is a specification from ASD, the Aerospace, Security and
Defence Industries Association of Europe. It started in the 1980s for
aircraft maintenance documents that non-native speakers read. ASD
released Issue 9 in January 2025. The specification has 53 rules
for text and a dictionary of approximately 900 approved words. Each
approved word has one part of speech and one meaning
(https://en.wikipedia.org/wiki/Simplified_Technical_English, as on
2026-11-26).

## Decision

Straum prose follows ASD-STE100. The rule applies to the guide, the
ADRs, `README.md`, the chart README, comments, error text, help text,
log lines and commit messages. AGENTS.md lists the rules for each day:
one topic in each sentence, the active voice and only simple tenses.
An instruction has 20 words at most, and a description has 25. A
paragraph has six sentences at most.

Each word has one meaning. A writer uses the STE approved word in its
approved sense, or a technical name. Each term in
`docs/src/glossary.md` is a technical name. A glossary term has
priority over a general word with a near meaning.

Code, commands, paths, SQL, flag names and quoted PostgreSQL text keep
their words. A quote from another source also keeps its words, as in
ADR-0019. STE applies only to the words around them. Names in code
follow ADR-0013, not STE.

Text from before 2026-11-25 does not follow the rule everywhere. A
commit that changes a sentence writes that sentence in STE. A commit
does not change other old text only for STE.

No script checks the rule. A script can count words and find some
passive forms, but it cannot compare each word with the dictionary. So
review holds the rule.

Straum does not take these alternatives:

- Plain English with no standard: it has no test, and each writer
  reads "plain" in a different way.
- A word list of our own: it takes work to keep current. STE already
  has a dictionary and rules that technical writers know.

## Consequences

- Each sentence has one topic, so a reader or a machine translation
  has less to guess.
- The glossary and STE together give one word for each thing.
- Text has more sentences, and some sentences sound stiff to a native
  reader.
- An error text or log line in STE can change the exact text that a
  test compares. The test changes in the same commit.
- Old text stays mixed until a commit changes it.
- Review holds the rule, so a long sentence or a synonym can pass.
