use anyhow::Result;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::{Repo, RepoType};
use std::path::PathBuf;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

const MODEL_ID: &str = "intfloat/multilingual-e5-small";

pub struct EmbeddingProvider {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl EmbeddingProvider {
    pub fn new(show_progress: bool) -> Result<Self> {
        let device = select_device()?;
        let (config_path, tokenizer_path, weights_path) = download_model(show_progress)?;

        let config: Config = serde_json::from_str(&std::fs::read_to_string(&config_path)?)?;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)? };
        let model = BertModel::load(vb, &config)?;

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams { max_length: 512, ..Default::default() }))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(Self { model, tokenizer, device })
    }

    pub fn embed_query(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.iter().map(|t| format!("query: {t}")).collect();
        let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
        self.embed_batch(&refs)
    }

    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
        let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
        self.embed_batch(&refs)
    }

    pub fn embed_documents_with_batch(
        &self,
        texts: &[String],
        batch_size: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let bs = batch_size.max(1);
        let mut all = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(bs) {
            all.extend(self.embed_documents(chunk)?);
        }
        Ok(all)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let token_ids = encodings
            .iter()
            .map(|e| Tensor::new(e.get_ids(), &self.device))
            .collect::<candle_core::Result<Vec<_>>>()?;
        let token_ids = Tensor::stack(&token_ids, 0)?;

        let attention_masks = encodings
            .iter()
            .map(|e| Tensor::new(e.get_attention_mask(), &self.device))
            .collect::<candle_core::Result<Vec<_>>>()?;
        let attention_mask = Tensor::stack(&attention_masks, 0)?;

        let token_type_ids = token_ids.zeros_like()?;

        let hidden = self.model.forward(&token_ids, &token_type_ids, Some(&attention_mask))?;

        let mask = attention_mask.to_dtype(DTYPE)?.unsqueeze(2)?;
        let sum_mask = mask.sum(1)?;
        let pooled = hidden.broadcast_mul(&mask)?.sum(1)?;
        let pooled = pooled.broadcast_div(&sum_mask)?;

        let norm = pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = pooled.broadcast_div(&norm)?;

        Ok(normalized.to_vec2::<f32>()?)
    }

    pub fn device_name(&self) -> &str {
        match &self.device {
            Device::Cpu => "CPU",
            Device::Cuda(_) => "CUDA",
            Device::Metal(_) => "Metal",
        }
    }
}

fn select_device() -> Result<Device> {
    if candle_core::utils::cuda_is_available() {
        Ok(Device::new_cuda(0)?)
    } else if candle_core::utils::metal_is_available() {
        Ok(Device::new_metal(0)?)
    } else {
        Ok(Device::Cpu)
    }
}

fn download_model(show_progress: bool) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let api = hf_hub::api::sync::ApiBuilder::from_env().build()?;
    let repo =
        api.repo(Repo::with_revision(MODEL_ID.to_string(), RepoType::Model, "main".to_string()));

    if show_progress {
        println!("Downloading model {MODEL_ID} (first run only)...");
    }

    let config = repo.get("config.json")?;
    let tokenizer = repo.get("tokenizer.json")?;
    let weights = repo.get("model.safetensors")?;

    Ok((config, tokenizer, weights))
}
