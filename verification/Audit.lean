import Clew

/-!
# What the proofs rest on

`lake env lean Audit.lean` prints, for every theorem the documentation
names, the axioms its proof uses. `check.sh` fails unless each uses only
Lean's three standard ones (`propext`, `Classical.choice`, `Quot.sound`) —
no `sorry`, and no `Lean.ofReduceBool`, the axiom that trusts compiled code
the kernel did not check.
-/

-- Path confinement
#print axioms Clew.Confine.accepted_stays_inside
#print axioms Clew.Confine.accepted_never_leaves
#print axioms Clew.Confine.checkLexical_spec
-- No downgrade
#print axioms Clew.Update.lt_irrefl
#print axioms Clew.Update.lt_trans
#print axioms Clew.Update.lt_asymm
#print axioms Clew.Update.lt_total
#print axioms Clew.Update.accepted_is_newer
#print axioms Clew.Update.installAll_chain
#print axioms Clew.Update.installs_never_repeat
#print axioms Clew.Update.parse_display
#print axioms Clew.Update.parse_tag
-- Which server binary may be deployed
#print axioms Clew.ServerDist.slug_is_a_token
#print axioms Clew.ServerDist.slug_has_no_path_or_url_syntax
#print axioms Clew.ServerDist.assetName_injective
#print axioms Clew.ServerDist.pinned_only
#print axioms Clew.ServerDist.sidecar_ignored
#print axioms Clew.ServerDist.unlisted_refused
#print axioms Clew.ServerDist.parseDigestList_wellformed
#print axioms Clew.ServerDist.release_list_parses
-- Consent
#print axioms Clew.Trust.argsPart_injective
#print axioms Clew.Trust.fields_injective
#print axioms Clew.Trust.command_ne_options
#print axioms Clew.Trust.command_ne_build
#print axioms Clew.Trust.options_ne_build
#print axioms Clew.Trust.commandInput_injective
#print axioms Clew.Trust.optionsInput_injective
#print axioms Clew.Trust.approvalKey_injective
#print axioms Clew.Trust.local_ne_remote
-- Framing
#print axioms Clew.Framing.readFrame_encode
#print axioms Clew.Framing.readFrame_nil
#print axioms Clew.Framing.readAll_encode
#print axioms Clew.Framing.readFrame_sound
#print axioms Clew.Framing.readJsonLine_line
#print axioms Clew.Framing.readLineCapped_within
#print axioms Clew.Framing.readLineCapped_over
-- Voice routing
#print axioms Clew.VoiceRoute.bye_first
#print axioms Clew.VoiceRoute.hush_never_asked
#print axioms Clew.VoiceRoute.hush_answers_the_question
#print axioms Clew.VoiceRoute.hush_never_yes
#print axioms Clew.VoiceRoute.runs_only_on_yes
#print axioms Clew.VoiceRoute.question_words_never_asked
#print axioms Clew.VoiceRoute.hush_stops_the_answer_first
#print axioms Clew.VoiceRoute.ends_in_one_act
#print axioms Clew.VoiceRoute.steps_have_their_subject
-- Shell words, ssh's arguments, known_hosts
#print axioms Clew.Ssh.readWord_quote
#print axioms Clew.Ssh.readWords_shellCommand
#print axioms Clew.Ssh.readWords_shC
#print axioms Clew.Ssh.parseSshArgs_sshArgs
#print axioms Clew.Ssh.sshEndpoint_sshArgs
#print axioms Clew.Ssh.clewSshLabel_sshArgs
#print axioms Clew.Ssh.plainHostField_literal
#print axioms Clew.Ssh.knownHostsPattern_plain
#print axioms Clew.Ssh.knownHostsPattern_injective
