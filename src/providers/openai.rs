use crate::providers::llm::Tokens;
use crate::providers::llm::{Generation, LLM};
use crate::providers::provider::Provider;
use crate::utils;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hyper::{body::Buf, Body, Client, Method, Request, Uri};
use hyper_tls::HttpsConnector;
use itertools::izip;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::prelude::*;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Logprobs {
    pub tokens: Vec<String>,
    pub token_logprobs: Vec<Option<f32>>,
    pub top_logprobs: Option<Vec<Option<HashMap<String, f32>>>>,
    pub text_offset: Vec<usize>,
}

impl Logprobs {
    pub fn logprob(&self) -> f32 {
        let mut logp = 0_f32;
        self.token_logprobs.iter().for_each(|lgp| match lgp {
            None => (),
            Some(lgp) => logp += lgp,
        });
        logp
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Choice {
    pub text: String,
    pub index: usize,
    pub logprobs: Option<Logprobs>,
    pub finish_reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Completion {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct InnerError {
    pub message: String,
    #[serde(alias = "type")]
    pub _type: String,
    pub param: Option<String>,
    pub code: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Error {
    pub error: InnerError,
}

pub struct OpenAILLM {
    model_id: String,
    api_key: Option<String>,
}

impl OpenAILLM {
    pub fn new(model_id: String) -> Self {
        OpenAILLM {
            model_id,
            api_key: None,
        }
    }

    fn uri(&self) -> Result<Uri> {
        Ok(format!("https://api.openai.com/v1/completions",).parse::<Uri>()?)
    }

    async fn completion(
        &self,
        prompt: String,
        max_tokens: Option<i32>,
        temperature: f32,
        n: usize,
        logprobs: Option<usize>,
        echo: bool,
        stop: Option<Vec<String>>,
    ) -> Result<Completion> {
        assert!(self.api_key.is_some());

        let https = HttpsConnector::new();
        let cli = Client::builder().build::<_, hyper::Body>(https);

        let req = Request::builder()
            .method(Method::POST)
            .uri(self.uri()?)
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.clone().unwrap()),
            )
            // .header("OpenAI-Organization", "openai")
            .body(Body::from(
                json!({
                    "model": self.model_id.clone(),
                    "prompt": prompt.as_str(),
                    "max_tokens": max_tokens,
                    "temperature": temperature,
                    "n": n,
                    "logprobs": logprobs,
                    "echo": echo,
                    "stop": stop,
                })
                .to_string(),
            ))?;

        let res = cli.request(req).await?;

        let body = hyper::body::aggregate(res).await?;
        let mut b: Vec<u8> = vec![];
        body.reader().read_to_end(&mut b)?;
        let c: &[u8] = &b;

        let completion: Completion = match serde_json::from_slice(c) {
            Ok(c) => Ok(c),
            Err(_) => {
                let error: Error = serde_json::from_slice(c)?;
                Err(anyhow!(
                    "OpenAIAPIError: [{}] {}",
                    error.error._type,
                    error.error.message
                ))
            }
        }?;

        Ok(completion)
    }
}

#[async_trait]
impl LLM for OpenAILLM {
    fn model_id(&self) -> String {
        self.model_id.clone()
    }

    fn name(&self) -> String {
        format!("llm.openai.{}", self.model_id)
    }

    fn initialize(&mut self) -> Result<()> {
        match std::env::var("OPENAI_API_KEY") {
            Ok(key) => {
                self.api_key = Some(key);
            }
            Err(_) => Err(anyhow!("Environment variable `OPENAI_API_KEY` is not set."))?,
        }
        Ok(())
    }

    async fn generate(
        &self,
        prompt: String,
        max_tokens: Option<i32>,
        temperature: f32,
        n: usize,
        stop: Option<Vec<String>>,
    ) -> Result<Generation> {
        assert!(n > 0);

        let c = self
            .completion(
                prompt.clone(),
                max_tokens,
                temperature,
                n,
                Some(0),
                true,
                stop,
            )
            .await?;

        assert!(c.choices.len() > 0);
        assert!(c.choices[0].logprobs.is_some());

        let logp = c.choices[0].logprobs.as_ref().unwrap();
        assert!(logp.tokens.len() == logp.token_logprobs.len());
        assert!(logp.tokens.len() == logp.text_offset.len());

        // UTF-8 length of the prompt (as used by the API for text_offset).
        let prompt_len = prompt.chars().count();

        let mut token_offset: usize = 0;

        let mut prompt_tokens = Tokens {
            text: prompt,
            tokens: Some(vec![]),
            logprobs: Some(vec![]),
        };
        for (o, t, l) in izip!(
            logp.text_offset.clone(),
            logp.tokens.clone(),
            logp.token_logprobs.clone()
        ) {
            if o < prompt_len {
                prompt_tokens.tokens.as_mut().unwrap().push(t.clone());
                prompt_tokens.logprobs.as_mut().unwrap().push(l);
                token_offset += 1;
            }
        }

        Ok(Generation {
            provider: String::from("fOO"),
            model: self.model_id.clone(),
            completions: c
                .choices
                .iter()
                .map(|c| {
                    let logp = c.logprobs.as_ref().unwrap();
                    assert!(logp.tokens.len() == logp.token_logprobs.len());
                    assert!(logp.tokens.len() == logp.text_offset.len());
                    assert!(logp.tokens.len() >= token_offset);

                    Tokens {
                        text: c.text.chars().skip(prompt_len).collect::<String>(),
                        tokens: Some(logp.tokens[token_offset..].to_vec()),
                        logprobs: Some(logp.token_logprobs[token_offset..].to_vec()),
                    }
                })
                .collect::<Vec<_>>(),
            prompt: prompt_tokens,
        })
    }
}

pub struct OpenAIProvider {}

impl OpenAIProvider {
    pub fn new() -> Self {
        OpenAIProvider {}
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn id(&self) -> String {
        String::from("openai")
    }

    fn setup(&self) -> Result<()> {
        utils::info("Setting up OpenAI:");
        utils::info("");
        utils::info(
            "To use OpenAI's models, you must set the environment variable `OPENAI_API_KEY`.",
        );
        utils::info("Your API key can be found at `https://openai.com/dashboard/settings/api`.");
        utils::info("");
        utils::info("Once ready you can check your setup with `dust provider test openai`");

        Ok(())
    }

    async fn test(&self) -> Result<()> {
        if !utils::confirm(
            "You are about to make a request for 1 tokens  to `text-ada-001` on the OpenAI API.",
        )? {
            Err(anyhow!("User aborted OpenAI test."))?;
        }

        let mut llm = self.llm(String::from("text-ada-001"));
        llm.initialize()?;

        let prompt = String::from("Hello 😊");
        let _ = llm.generate(prompt, Some(1), 0.7, 1, None).await?;

        utils::done("Test successfully completed! OpenAI is ready to use.");

        Ok(())
    }

    fn llm(&self, model_id: String) -> Box<dyn LLM + Sync + Send> {
        Box::new(OpenAILLM::new(model_id))
    }
}