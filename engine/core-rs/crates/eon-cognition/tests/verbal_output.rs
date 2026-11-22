mod persona_tests {
    use eon_cognition::persona::{SpeechAct, format_speech};
    use eon_cognition::prompts::speech_instructions;

    #[test]
    fn public_api_renders_each_act_exactly_once_and_is_idempotent() {
        for (act, prefix) in [
            (SpeechAct::Affirmative, "Affirmative."),
            (SpeechAct::Report, "Report:"),
            (SpeechAct::Notice, "Notice:"),
            (SpeechAct::Negative, "Negative."),
            (SpeechAct::Proposal, "Proposal:"),
        ] {
            let speech = format_speech(act, "\tDisk\n  2:  résumé.txt  ").unwrap();
            assert_eq!(speech, format!("{prefix} Disk 2: résumé.txt"));
            assert_eq!(format_speech(act, &speech).unwrap(), speech);
            assert!(speech_instructions(act).contains(&format!("exactly {prefix} once")));
        }
    }

    #[test]
    fn repeated_labels_and_stacked_stock_leads_do_not_become_speech() {
        let input = "Hello! How can I help you today? REPORT : Certainly, I'd be glad to help! \
                     report: Sure thing! 4 tests passed.";
        assert_eq!(
            format_speech(SpeechAct::Report, input).unwrap(),
            "Report: 4 tests passed."
        );
        assert_eq!(
            format_speech(SpeechAct::Negative, "Negative: NEGATIVE. File missing.").unwrap(),
            "Negative. File missing."
        );
        assert_eq!(
            format_speech(
                SpeechAct::Affirmative,
                "Affirmative.Affirmative.Starting sync."
            )
            .unwrap(),
            "Affirmative. Starting sync."
        );
    }

    #[test]
    fn adjacent_identical_sentences_are_not_spoken_twice() {
        let input = "Battery at 9.8%. Battery at 9.8%. Backup pending.";
        let expected = "Notice: Battery at 9.8%. Backup pending.";
        assert_eq!(format_speech(SpeechAct::Notice, input).unwrap(), expected);
    }

    #[test]
    fn does_not_relabel_failures_or_speak_boilerplate_later_in_a_turn() {
        for text in [
            "Hello! Negative. File not found.",
            "Report: Ready. Report: Ready.",
            "Ready.Notice : new act.",
            "As an AI language model, I cannot access that.",
            "Three files found. Would you like me to read them?",
            "Report: Done! I hope this helps.",
            "Ready. Let me know if you need anything else.",
            "Report: <think>withheld</think> Done.",
            "Report: <thi",
            "Report: hidden\0control",
        ] {
            assert!(format_speech(SpeechAct::Report, text).is_err());
        }
    }

    #[test]
    fn only_a_bare_acknowledgment_can_have_no_body() {
        assert_eq!(
            format_speech(SpeechAct::Affirmative, "").unwrap(),
            "Affirmative."
        );
        for text in ["", "  \t\n", "Report:", "Hi there!", "Report: ..."] {
            assert!(format_speech(SpeechAct::Report, text).is_err());
        }
    }

    #[test]
    fn factual_substrings_survive() {
        let fact = "High pressure. Make sure report.csv is on disk at 09:15.";
        assert_eq!(
            format_speech(SpeechAct::Notice, fact).unwrap(),
            format!("Notice: {fact}")
        );
    }
}

mod router_tests {
    use eon_cognition::EonResult;
    use eon_cognition::persona::SpeechAct;
    use eon_cognition::router::SpeechRouter;

    fn route(pieces: &[&[u8]]) -> EonResult<String> {
        let mut router = SpeechRouter::new(SpeechAct::Report);
        for piece in pieces {
            router.push(piece)?;
        }
        router.finish()
    }

    #[test]
    fn every_byte_split_including_markers_prefix_and_utf8_gives_only_public_text() {
        let input =
            " \n<think>秘密</think>\n<think></think> Report: Hello! REPORT: Café prêt.".as_bytes();
        let expected = "Report: Café prêt.";
        for split in 0..=input.len() {
            assert_eq!(
                route(&[&input[..split], &input[split..]]).unwrap(),
                expected
            );
        }
        assert_eq!(
            route(&input.chunks(1).collect::<Vec<_>>()).unwrap(),
            expected
        );
    }

    #[test]
    fn non_thinking_turns_need_the_requested_public_opener() {
        assert_eq!(
            route(&[b"  Rep", b"ort: 8 nodes."]).unwrap(),
            "Report: 8 nodes."
        );
        for invalid in [
            b"raw unlabelled output".as_slice(),
            b"Notice: different act",
            b"Rep",
            b"REPORT:",
        ] {
            assert!(route(&[invalid]).is_err());
        }
    }

    #[test]
    fn a_label_inside_a_hidden_block_cannot_select_or_supply_the_public_act() {
        for act in [
            SpeechAct::Affirmative,
            SpeechAct::Report,
            SpeechAct::Notice,
            SpeechAct::Negative,
            SpeechAct::Proposal,
        ] {
            let public = format!("{} Access is restricted.", act.prefix());
            for lead in ["", "<think>Report: not for speech.</think>\n"] {
                let mut router = SpeechRouter::new(act);
                router.push(lead.as_bytes()).unwrap();
                router.push(public.as_bytes()).unwrap();
                assert_eq!(router.finish().unwrap(), public);
            }
            let mut hidden_only = SpeechRouter::new(act);
            hidden_only.push(b"<think>").unwrap();
            hidden_only.push(public.as_bytes()).unwrap();
            hidden_only.push(b"</think>").unwrap();
            assert!(hidden_only.finish().is_err());
        }
    }

    #[test]
    fn malformed_markers_never_release_text_even_when_split() {
        for turn in [
            "<thi",
            "<think>PRIVATE",
            "<Think>PRIVATE</Think>Report: OK",
            "<think extra>PRIVATE</think>Report: OK",
            "<think>PRIVATE</thin",
            "<think>PRIVATE</thinx>Report: OK",
            "<think><think>PRIVATE</think></think>Report: OK",
            "<think>PRIVATE</think>",
            "</think>Report: PRIVATE",
            "Report: PRIVATE</think>Report: OK",
            "Report: OK <think>PRIVATE</think>",
            "Report: OK <|im_start|>PRIVATE",
            "Report: OK think>PRIVATE",
            "Report: OK </",
        ] {
            for split in 0..=turn.len() {
                let bytes = turn.as_bytes();
                let err = route(&[&bytes[..split], &bytes[split..]]).unwrap_err();
                assert!(!err.to_string().contains("PRIVATE"));
            }
        }
    }

    #[test]
    fn failure_is_terminal_and_invalid_public_utf8_is_not_replaced() {
        let mut router = SpeechRouter::new(SpeechAct::Affirmative);
        assert!(router.push(b"</").is_err());
        assert!(router.push(b"Affirmative.").is_err());
        assert!(router.finish().is_err());
        assert!(route(&[b"Report: \xff"]).is_err());
        assert_eq!(
            route(&[b"<think>\xff</think>Report: Valid"]).unwrap(),
            "Report: Valid"
        );
    }

    #[test]
    fn large_hidden_blocks_are_discarded_and_public_turns_are_bounded() {
        let data = vec![b'x'; 40_000];
        assert_eq!(
            route(&[b"<think>", &data, b"</think>Report: Done"]).unwrap(),
            "Report: Done"
        );
        assert!(route(&[b"Report: ", &data]).is_err());
    }
}
