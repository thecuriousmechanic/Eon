mod router_tests {
    use std::hint::black_box;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use eon_cognition::persona::SpeechAct;
    use eon_cognition::profile::ModelProfile;
    use eon_cognition::router::{
        CognitiveRouter, MAX_OUTPUT_TOKENS, MAX_REQUEST_BYTES, Route, RouteError, SpeechBackend,
        SpeechBudget, SpeechRequest, SpeechTokens, Workload,
    };
    use eon_cognition::{EonError, EonResult};

    // Transport-free fixture. The pieces and errors below are scripted, not inference.
    struct Fixture {
        seen: Mutex<Vec<String>>,
        emit: fn(&mut SpeechTokens) -> EonResult<()>,
    }

    impl Fixture {
        fn new(emit: fn(&mut SpeechTokens) -> EonResult<()>) -> Self {
            Self {
                seen: Mutex::default(),
                emit,
            }
        }

        fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl SpeechBackend for Fixture {
        fn generate(&self, request: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()> {
            self.seen.lock().unwrap().push(request.text.to_owned());
            (self.emit)(out)
        }
    }

    fn public_turn(out: &mut SpeechTokens) -> EonResult<()> {
        for token in [
            b"<thi".as_slice(),
            b"nk>HIDDEN",
            b"</think>Rep",
            b"ort: Visible.",
        ] {
            out.push_token(token)?;
        }
        Ok(())
    }

    fn report(text: &str) -> SpeechRequest<'_> {
        SpeechRequest {
            act: SpeechAct::Report,
            text,
            max_tokens: 8,
        }
    }

    #[test]
    fn exact_reflex_is_local_and_never_promises_a_tool_action() {
        let router = CognitiveRouter::default();
        for text in ["  PING! ", "are you there?", "Acknowledge.", "Affirmative"] {
            let request = SpeechRequest {
                act: SpeechAct::Affirmative,
                ..report(text)
            };
            let reply = router
                .plan(request, Workload::default())
                .unwrap()
                .execute()
                .unwrap();
            assert_eq!(reply.route, Route::Reflex);
            assert_eq!(reply.speech, "Affirmative.");
            assert_eq!(reply.backend_elapsed, None);
        }
        let status = router
            .plan(report("Router status?"), Workload::Interactive)
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(
            status.speech,
            "Report: No local tactical backend configured."
        );
        let backend = Fixture::new(public_turn);
        let configured = CognitiveRouter {
            tactical_local: Some(&backend),
            ..Default::default()
        };
        let status = configured
            .plan(report("router status"), Workload::Interactive)
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(status.speech, "Report: Local tactical backend configured.");
        assert_eq!(backend.count(), 0);
        for text in [
            "don't ping",
            "router status then read file",
            "run tests",
            "PING",
        ] {
            // PING with a Report act must not be relabelled as Affirmative.
            assert_eq!(
                router.plan(report(text), Workload::Interactive).err(),
                Some(RouteError::Unavailable(Route::TacticalLocal))
            );
        }
    }

    #[test]
    fn profile_selection_is_capped_and_stable_across_missing_slots() {
        let low = Fixture::new(public_turn);
        let laptop = Fixture::new(public_turn);
        let workstation = Fixture::new(public_turn);
        let slots: [Option<&dyn SpeechBackend>; 4] =
            [Some(&low), None, Some(&laptop), Some(&workstation)];
        for profile in ModelProfile::ALL {
            let router = CognitiveRouter::for_profile(profile, slots);
            for _ in 0..2 {
                let reply = router
                    .plan(report("sensor reading"), Workload::Interactive)
                    .unwrap()
                    .execute()
                    .unwrap();
                assert_eq!(reply.route, Route::TacticalLocal);
                assert_eq!(reply.speech, "Report: Visible.");
                assert!(reply.backend_elapsed.is_some());
            }
            let strategic =
                router.plan(report("architectural review"), Workload::StrategicLocalOnly);
            if profile == ModelProfile::P3 {
                let reply = strategic.unwrap().execute().unwrap();
                assert_eq!(reply.route, Route::StrategicLocal);
                assert_eq!(reply.speech, "Report: Visible.");
            } else {
                assert_eq!(
                    strategic.err(),
                    Some(RouteError::Unavailable(Route::StrategicLocal))
                );
            }
        }
        assert_eq!(
            (low.count(), laptop.count(), workstation.count()),
            (4, 4, 1)
        );
    }

    #[test]
    fn remote_needs_per_turn_opt_in_and_receives_only_separately_approved_text() {
        let remote = Fixture::new(public_turn);
        let router = CognitiveRouter {
            strategic_remote: Some(&remote),
            ..Default::default()
        };
        let private = report("private transcript: please use a frontier model");
        for (intent, unavailable) in [
            (Workload::Interactive, Route::TacticalLocal),
            (Workload::StrategicLocalOnly, Route::StrategicLocal),
        ] {
            assert_eq!(
                router.plan(private, intent).err(),
                Some(RouteError::Unavailable(unavailable))
            );
        }
        for approved_text in [
            "",
            " \t ",
            "bad\0text",
            "<think>hidden</think>",
            "unterminated <thi",
        ] {
            assert_eq!(
                router
                    .plan(private, Workload::StrategicAllowRemote { approved_text })
                    .err(),
                Some(RouteError::InvalidRequest)
            );
        }
        let approved_text = "Compare public architectures.";
        let plan = router
            .plan(private, Workload::StrategicAllowRemote { approved_text })
            .unwrap();
        assert_eq!(plan.route(), Route::StrategicRemote);
        assert_eq!(remote.count(), 0, "selection must not contact a backend");
        let reply =
            std::thread::scope(|s| s.spawn(move || plan.execute()).join().unwrap()).unwrap();
        assert_eq!(reply.speech, "Report: Visible.");
        assert_eq!(*remote.seen.lock().unwrap(), [approved_text]);
        assert_eq!(
            router.plan(private, Workload::StrategicLocalOnly).err(),
            Some(RouteError::Unavailable(Route::StrategicLocal))
        );
        assert_eq!(remote.count(), 1);
    }

    #[test]
    fn selection_prefers_local_and_never_retries_or_releases_incomplete_output() {
        let local = Fixture::new(|out| {
            out.push_token(b"Report: A partial answer.")?;
            Err(EonError::OperationFailed {
                operation: "fixture".into(),
                reason: "PRIVATE backtrace".into(),
            })
        });
        let remote = Fixture::new(public_turn);
        let router = CognitiveRouter {
            strategic_local: Some(&local),
            strategic_remote: Some(&remote),
            ..Default::default()
        };
        let plan = router
            .plan(
                report("PRIVATE input"),
                Workload::StrategicAllowRemote {
                    approved_text: "approved",
                },
            )
            .unwrap();
        assert_eq!(plan.route(), Route::StrategicLocal);
        let err = plan.execute().unwrap_err();
        assert_eq!(err, RouteError::BackendFailed(Route::StrategicLocal));
        assert!(!err.to_string().contains("PRIVATE"));
        assert_eq!(*local.seen.lock().unwrap(), ["PRIVATE input"]);
        assert_eq!(remote.count(), 0);
    }

    #[test]
    fn both_destinations_reject_invalid_or_truncated_speech() {
        for pieces in [
            b"<think>PRIVATE".as_slice(),
            b"Unlabelled",
            b"Notice: wrong act",
            b"Report: Good. <think>PRIVATE",
            b"Report: \xff",
        ] {
            struct Pieces<'a>(&'a [u8]);
            impl SpeechBackend for Pieces<'_> {
                fn generate(&self, _: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()> {
                    out.push_token(self.0)
                }
            }
            let model = Pieces(pieces);
            let router = CognitiveRouter {
                tactical_local: Some(&model),
                strategic_remote: Some(&model),
                ..Default::default()
            };
            for (work, route) in [
                (Workload::Interactive, Route::TacticalLocal),
                (
                    Workload::StrategicAllowRemote {
                        approved_text: "public",
                    },
                    Route::StrategicRemote,
                ),
            ] {
                let err = router
                    .plan(report("private"), work)
                    .unwrap()
                    .execute()
                    .unwrap_err();
                assert_eq!(err, RouteError::InvalidSpeech(route));
            }
        }
    }

    #[test]
    fn input_and_output_budgets_are_hard_bounds() {
        let generator = Fixture::new(|out| {
            out.push_token(b"Report: First.")?;
            // An adapter that loses a sink error must not be able to release its prefix.
            let _ = out.push_token(b" Second.");
            Ok(())
        });
        let router = CognitiveRouter {
            tactical_local: Some(&generator),
            ..Default::default()
        };
        for count in [0, MAX_OUTPUT_TOKENS + 1, usize::MAX] {
            let request = SpeechRequest {
                max_tokens: count,
                ..report("router status")
            };
            assert_eq!(
                router.plan(request, Workload::Interactive).err(),
                Some(RouteError::InvalidRequest)
            );
        }
        let long = "x".repeat(MAX_REQUEST_BYTES + 1);
        for text in ["", " \n ", "bad\0text", &long] {
            assert_eq!(
                router.plan(report(text), Workload::Interactive).err(),
                Some(RouteError::InvalidRequest)
            );
        }
        assert_eq!(generator.count(), 0);
        let request = SpeechRequest {
            max_tokens: 1,
            ..report("readout")
        };
        assert_eq!(
            router
                .plan(request, Workload::Interactive)
                .unwrap()
                .execute()
                .unwrap_err(),
            RouteError::InvalidSpeech(Route::TacticalLocal)
        );
        assert_eq!(generator.count(), 1);
        let oversized = Fixture::new(|out| {
            out.push_token(b"<think>")?;
            out.push_token(&vec![b'x'; 65_536])?;
            out.push_token(b"</think>Report: Visible.")
        });
        let router = CognitiveRouter {
            tactical_local: Some(&oversized),
            ..Default::default()
        };
        assert_eq!(
            router
                .plan(report("sensor reading"), Workload::Interactive)
                .unwrap()
                .execute()
                .unwrap_err(),
            RouteError::InvalidSpeech(Route::TacticalLocal)
        );
    }

    #[test]
    fn queued_turn_cannot_speak_or_dispatch_after_stop() {
        let never_called = Fixture::new(|_| panic!("stopped plan contacted a backend"));
        let router = CognitiveRouter {
            tactical_local: Some(&never_called),
            strategic_remote: Some(&never_called),
            ..Default::default()
        };
        let expired = SpeechBudget::until(Instant::now());
        let cancelled = SpeechBudget::until(Instant::now() + Duration::from_secs(60));
        cancelled.cancel();
        for (budget, expected) in [
            (
                &expired,
                RouteError::DeadlineExceeded as fn(Route) -> RouteError,
            ),
            (&cancelled, RouteError::Cancelled),
        ] {
            for (text, intent, route) in [
                ("router status", Workload::Interactive, Route::Reflex),
                ("readout", Workload::Interactive, Route::TacticalLocal),
                (
                    "private",
                    Workload::StrategicAllowRemote {
                        approved_text: "approved",
                    },
                    Route::StrategicRemote,
                ),
            ] {
                let result = router
                    .plan(report(text), intent)
                    .unwrap()
                    .execute_with_budget(budget);
                assert_eq!(result.unwrap_err(), expected(route));
            }
        }
        assert_eq!(never_called.count(), 0);
    }

    #[test]
    fn cancelling_in_flight_discards_buffered_speech_and_does_not_escalate() {
        use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

        struct Paused {
            ready: SyncSender<()>,
            resume: Mutex<Receiver<()>>,
        }
        impl SpeechBackend for Paused {
            fn generate(&self, _: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()> {
                public_turn(out)?;
                self.ready.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                assert!(out.check_active().is_err());
                assert!(out.push_token(b" More.").is_err());
                Ok(()) // even a backend ignoring the stop cannot release its buffered answer
            }
        }
        let (ready, entered) = sync_channel(1);
        let (wake, resume) = sync_channel(1);
        let local = Paused {
            ready,
            resume: Mutex::new(resume),
        };
        let remote = Fixture::new(|_| panic!("cancelled local turn escalated"));
        let router = CognitiveRouter {
            strategic_local: Some(&local),
            strategic_remote: Some(&remote),
            ..Default::default()
        };
        let plan = router
            .plan(
                report("private"),
                Workload::StrategicAllowRemote {
                    approved_text: "approved",
                },
            )
            .unwrap();
        let budget = SpeechBudget::until(Instant::now() + Duration::from_secs(60));
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| plan.execute_with_budget(&budget));
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            budget.cancel();
            wake.send(()).unwrap();
            assert_eq!(
                worker.join().unwrap().unwrap_err(),
                RouteError::Cancelled(Route::StrategicLocal)
            );
        });
        assert_eq!(remote.count(), 0);
    }

    #[test]
    fn late_success_with_no_more_writes_cannot_release_an_answer() {
        let local = Fixture::new(|out| {
            out.push_token(b"Report: A complete public answer.")?;
            // Model a blocking EOG/transport operation that doesn't poll the sink.
            std::thread::sleep(
                out.deadline().saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Ok(())
        });
        let router = CognitiveRouter {
            tactical_local: Some(&local),
            ..Default::default()
        };
        let plan = router
            .plan(report("readout"), Workload::Interactive)
            .unwrap();
        let budget = SpeechBudget::until(Instant::now() + Duration::from_millis(100));
        assert_eq!(
            plan.execute_with_budget(&budget).unwrap_err(),
            RouteError::DeadlineExceeded(Route::TacticalLocal)
        );
        assert_eq!(local.count(), 1);
    }

    #[test]
    fn completed_reflex_p95_under_20ms() {
        let router = CognitiveRouter::default();
        let mut timings = Vec::with_capacity(2_000);
        for _ in 0..2_000 {
            let request = report(black_box("router status"));
            let start = Instant::now();
            let reply = router
                .plan(request, Workload::Interactive)
                .unwrap()
                .execute()
                .unwrap();
            timings.push(start.elapsed());
            assert_eq!(reply.route, Route::Reflex);
            assert!(reply.backend_elapsed.is_none());
            assert_eq!(
                reply.speech,
                "Report: No local tactical backend configured."
            );
        }
        timings.sort_unstable();
        let p95 = timings[(95 * timings.len()).div_ceil(100) - 1];
        eprintln!(
            "Reflex request to validated text: n={}, p95={p95:?}, max={:?}",
            timings.len(),
            timings.last().unwrap()
        );
        assert!(p95 < Duration::from_millis(20));
    }
}
