/// Age and gender estimation via Gemini Vision API.
///
/// Called once per frame (with a 5-second timeout). If the API is unavailable
/// or the key is not configured, returns `Unknown` placeholders.
use shared::{AgeGroup, DemographicEstimate, GenderEstimate};

/// Estimate demographics for people detected in a JPEG frame.
///
/// `people_count` is the number of people already detected by YOLOv8 so the
/// result is padded / truncated to match.
pub async fn estimate(
    jpeg_bytes: &[u8],
    people_count: u32,
    gemini_key: &str,
) -> Vec<DemographicEstimate> {
    if people_count == 0 || jpeg_bytes.is_empty() {
        return vec![];
    }

    use base64::Engine;
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes);

    let prompt = "Store security camera image. \
        For each visible person estimate age_group (child/teen/young_adult/adult/senior) \
        and gender (male/female/unknown). \
        Reply ONLY with a compact JSON array, e.g. \
        [{\"age\":\"adult\",\"gender\":\"male\"},{\"age\":\"young_adult\",\"gender\":\"female\"}]. \
        Return [] if no people are visible.";

    let body = serde_json::json!({
        "contents": [{
            "parts": [
                {
                    "inline_data": {
                        "mime_type": "image/jpeg",
                        "data": image_b64
                    }
                },
                { "text": prompt }
            ]
        }],
        "generationConfig": {
            "maxOutputTokens": 256,
            "temperature": 0.1,
            "responseMimeType": "text/plain"
        }
    });

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={gemini_key}"
    );

    let client = reqwest::Client::new();
    let resp = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.post(&url).json(&body).send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        _ => return fallback(people_count),
    };

    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return fallback(people_count),
    };

    parse_response(&text, people_count)
}

fn parse_response(text: &str, people_count: u32) -> Vec<DemographicEstimate> {
    // Extract the first JSON array from the response body.
    let start = match text.find('[') {
        Some(i) => i,
        None => return fallback(people_count),
    };
    let end = match text[start..].rfind(']') {
        Some(i) => start + i,
        None => return fallback(people_count),
    };

    let json_str = &text[start..=end];
    let arr: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return fallback(people_count),
    };

    let mut result: Vec<DemographicEstimate> = arr
        .iter()
        .map(|item| {
            let age = item["age"].as_str().unwrap_or("adult");
            let gender = item["gender"].as_str().unwrap_or("unknown");
            DemographicEstimate {
                age_group: parse_age(age),
                gender: parse_gender(gender),
                confidence: 0.80,
            }
        })
        .collect();

    // Pad or truncate to match people_count.
    while result.len() < people_count as usize {
        result.push(DemographicEstimate {
            age_group: AgeGroup::Adult,
            gender: GenderEstimate::Unknown,
            confidence: 0.40,
        });
    }
    result.truncate(people_count as usize);
    result
}

fn parse_age(s: &str) -> AgeGroup {
    match s {
        "child" => AgeGroup::Child,
        "teen" => AgeGroup::Teen,
        "young_adult" => AgeGroup::YoungAdult,
        "adult" => AgeGroup::Adult,
        "senior" => AgeGroup::Senior,
        _ => AgeGroup::Adult,
    }
}

fn parse_gender(s: &str) -> GenderEstimate {
    match s {
        "male" => GenderEstimate::Male,
        "female" => GenderEstimate::Female,
        _ => GenderEstimate::Unknown,
    }
}

pub fn fallback(people_count: u32) -> Vec<DemographicEstimate> {
    (0..people_count)
        .map(|_| DemographicEstimate {
            age_group: AgeGroup::Adult,
            gender: GenderEstimate::Unknown,
            confidence: 0.30,
        })
        .collect()
}
